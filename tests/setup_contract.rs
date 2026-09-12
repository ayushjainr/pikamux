use pikamux::model::Provider;
use pikamux::setup::{
    FileChange, SetupOptions, SetupPaths, apply_changes, claude_settings_change,
    codex_config_change, codex_hooks_change, hooks_installed, opencode_plugin_change,
    pika_config_change, proposed_hook_changes,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn executable(root: &Path) -> PathBuf {
    let path = root.join("bin/pika with space");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn options(executable: PathBuf) -> SetupOptions {
    SetupOptions {
        default_provider: Provider::Claude,
        machine_alias: Some("laptop".into()),
        provider_executables: BTreeMap::from([
            ("codex".into(), "/opt/codex".into()),
            ("claude".into(), "/opt/claude".into()),
        ]),
        provider_runtime_path: Some("/opt/bin:/usr/bin".into()),
        pika_executable: executable,
    }
}

#[test]
fn json_hooks_preserve_foreign_handlers_quote_paths_and_are_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let codex = temp.path().join("codex");
    fs::create_dir_all(&codex).unwrap();
    fs::write(
        codex.join("hooks.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing"}]}]}}"#,
    )
    .unwrap();
    let change = codex_hooks_change(&codex, &binary).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&change.after).unwrap();
    assert_eq!(
        parsed["hooks"]["Stop"][0]["hooks"][0]["command"],
        "existing"
    );
    let command = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.starts_with('\''));
    assert_eq!(
        shell_words::split(command).unwrap()[0],
        binary.to_string_lossy()
    );
    apply_changes(&[change], "20260911T120000Z").unwrap();
    assert!(!codex_hooks_change(&codex, &binary).unwrap().changed());
    assert!(hooks_installed(&codex, Provider::Codex, &binary));
}

#[test]
fn stale_python_handler_is_replaced_and_claude_hooks_are_reenabled() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let claude = temp.path().join("claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"model":"opus","disableAllHooks":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/old/python -m pikamux hook --provider claude"}]}]}}"#,
    )
    .unwrap();
    let change = claude_settings_change(&claude, &binary).unwrap();
    let value: serde_json::Value = serde_json::from_str(&change.after).unwrap();
    assert_eq!(value["model"], "opus");
    assert_eq!(value["disableAllHooks"], false);
    assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert!(!change.after.contains("/old/python"));
    apply_changes(&[change], "one").unwrap();
    assert!(hooks_installed(&claude, Provider::Claude, &binary));
}

#[test]
fn codex_toml_merge_preserves_comments_and_rejects_invalid_input() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "[features] # lifecycle\nhooks_extra = false\nhooks = false # pika\n\n[other]\nx = 1\n",
    )
    .unwrap();
    let change = codex_config_change(&home).unwrap();
    assert!(change.after.contains("hooks = true # pika"));
    assert!(change.after.contains("hooks_extra = false"));
    assert_eq!(change.after.matches("[features]").count(), 1);
    fs::write(home.join("config.toml"), "model = \"unterminated\n").unwrap();
    assert!(
        codex_config_change(&home)
            .unwrap_err()
            .to_string()
            .contains("invalid TOML")
    );
}

#[test]
fn pika_config_preserves_unknown_fields_and_existing_top_level_order() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let path = temp.path().join("pika/config.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "{\n  \"zeta\": 1,\n  \"future\": {\"x\": true},\n  \"default_provider\": \"codex\"\n}\n",
    )
    .unwrap();
    let change = pika_config_change(&path, &options(binary)).unwrap();
    assert!(change.after.find("\"zeta\"").unwrap() < change.after.find("\"future\"").unwrap());
    let value: serde_json::Value = serde_json::from_str(&change.after).unwrap();
    assert_eq!(value["future"]["x"], true);
    assert_eq!(value["default_provider"], "claude");
    assert_eq!(value["machine_alias"], "laptop");
}

#[test]
fn invalid_json_fails_closed_before_a_change_is_planned() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let home = temp.path().join("codex");
    fs::create_dir_all(&home).unwrap();
    let path = home.join("hooks.json");
    fs::write(&path, "{broken").unwrap();
    assert!(
        codex_hooks_change(&home, &binary)
            .unwrap_err()
            .to_string()
            .contains("invalid JSON")
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "{broken");
}

#[test]
fn opencode_plugin_maps_attention_and_verifies_native_rename() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let home = temp.path().join("opencode");
    let change = opencode_plugin_change(&home, &binary).unwrap();
    assert!(change.after.contains("QuestionRequest"));
    assert!(change.after.contains("SessionHeartbeat"));
    assert!(change.after.contains("client.session.update"));
    assert!(change.after.contains("observedTitle !== desired"));
    assert!(change.after.contains("p.info.parentID"));
    assert!(
        change
            .after
            .contains(&serde_json::to_string(&binary.to_string_lossy()).unwrap())
    );
    apply_changes(&[change], "plugin").unwrap();
    assert!(hooks_installed(&home, Provider::Opencode, &binary));
    if Command::new("node").arg("--version").output().is_ok() {
        let output = Command::new("node")
            .arg("--check")
            .arg(home.join("plugins/pika.js"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn stale_preview_stops_the_whole_batch_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    fs::write(&second, "before").unwrap();
    let changes = [
        FileChange {
            path: first.clone(),
            before: String::new(),
            after: "planned".into(),
            notice: None,
        },
        FileChange {
            path: second.clone(),
            before: "before".into(),
            after: "after".into(),
            notice: None,
        },
    ];
    fs::write(&second, "user edit").unwrap();
    assert!(
        apply_changes(&changes, "stale")
            .unwrap_err()
            .to_string()
            .contains("changed since preview")
    );
    assert!(!first.exists());
    assert_eq!(fs::read_to_string(second).unwrap(), "user edit");
}

#[cfg(unix)]
#[test]
fn symlink_target_is_never_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let external = temp.path().join("external");
    let target = temp.path().join("target");
    fs::write(&external, "keep").unwrap();
    symlink(&external, &target).unwrap();
    let change = FileChange {
        path: target,
        before: "keep".into(),
        after: "replace".into(),
        notice: None,
    };
    assert!(
        apply_changes(&[change], "link")
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert_eq!(fs::read_to_string(external).unwrap(), "keep");
}

#[test]
fn writes_are_private_and_collision_safe_backups_restore_exact_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("settings.json");
    fs::write(&target, "one\n").unwrap();
    let first = apply_changes(
        &[FileChange {
            path: target.clone(),
            before: "one\n".into(),
            after: "two\n".into(),
            notice: None,
        }],
        "same",
    )
    .unwrap();
    let second = apply_changes(
        &[FileChange {
            path: target.clone(),
            before: "two\n".into(),
            after: "three\n".into(),
            notice: None,
        }],
        "same",
    )
    .unwrap();
    assert_ne!(first.backups[0], second.backups[0]);
    assert_eq!(fs::read_to_string(&first.backups[0]).unwrap(), "one\n");
    assert_eq!(fs::read_to_string(&second.backups[0]).unwrap(), "two\n");
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn duplicate_targets_are_rejected_before_writing() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("same");
    let changes = [
        FileChange {
            path: target.clone(),
            before: String::new(),
            after: "one".into(),
            notice: None,
        },
        FileChange {
            path: target.clone(),
            before: String::new(),
            after: "two".into(),
            notice: None,
        },
    ];
    assert!(
        apply_changes(&changes, "duplicate")
            .unwrap_err()
            .to_string()
            .contains("duplicate target")
    );
    assert!(!target.exists());
}

#[test]
fn full_preview_is_read_only_then_applies_idempotently() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let roots = SetupPaths {
        pika_config: temp.path().join("pika/config.json"),
        codex_home: temp.path().join("codex"),
        claude_home: temp.path().join("claude"),
        opencode_config_home: temp.path().join("opencode"),
    };
    let planned = proposed_hook_changes(&roots, &options(binary.clone())).unwrap();
    assert_eq!(planned.len(), 5);
    assert!(!roots.pika_config.exists());
    assert!(!roots.codex_home.exists());
    assert!(!roots.claude_home.exists());
    assert!(!roots.opencode_config_home.exists());
    let receipt = apply_changes(&planned, "batch").unwrap();
    assert_eq!(receipt.written.len(), 5);
    assert!(
        proposed_hook_changes(&roots, &options(binary))
            .unwrap()
            .iter()
            .all(|change| !change.changed())
    );
}
