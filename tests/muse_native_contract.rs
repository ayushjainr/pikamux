//! Opt-in native provider check: disposable settings and the offline echo backend.
#![cfg(unix)]

use pikamux::{
    model::Provider,
    setup::{SetupOptions, SetupPaths, proposed_hook_changes},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires PIKA_TEST_MUSE_NATIVE pointing to an installed native Muse binary, not its updater launcher"]
fn native_echo_accepts_generated_user_hooks_without_model_quota() {
    let native =
        PathBuf::from(std::env::var_os("PIKA_TEST_MUSE_NATIVE").expect("native binary path"));
    assert!(native.is_absolute() && native.is_file());
    assert!(
        native
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("muse-bin-")
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = root.join("config");
    let muse = config.join("muse");
    fs::create_dir_all(&muse).unwrap();
    let capture = root.join("pika");
    let events = root.join("events.jsonl");
    fs::write(
        &capture,
        format!(
            "#!/bin/sh\ncat >> {}\nprintf '\\n' >> {}\n",
            shell_words::quote(events.to_str().unwrap()),
            shell_words::quote(events.to_str().unwrap())
        ),
    )
    .unwrap();
    fs::set_permissions(&capture, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = SetupPaths {
        pika_config: root.join("pika-config/config.json"),
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_config_home: config.join("opencode"),
        muse_config_home: muse.clone(),
    };
    let options = SetupOptions {
        default_provider: Provider::Muse,
        machine_alias: None,
        provider_executables: BTreeMap::from([(
            "muse".into(),
            native.to_string_lossy().into_owned(),
        )]),
        provider_runtime_path: Some("/usr/bin:/bin".into()),
        pika_executable: capture,
    };
    let changes = proposed_hook_changes(&paths, &options).unwrap();
    let settings = changes
        .into_iter()
        .find(|change| change.path == muse.join("settings.json"))
        .unwrap();
    fs::write(&settings.path, settings.after).unwrap();
    let mut child = Command::new(&native)
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .current_dir(root)
        .args(["exec", "--provider", "echo", "--workspace"])
        .arg(root)
        .args([
            "--no-foreign-personal-context",
            "--disable-web-tools",
            "--disable-shell",
            "--disable-write",
            "--max-model-steps",
            "1",
            "--json",
            "Pika offline compatibility fixture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "native echo failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native echo exceeded 30 seconds");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let events: Vec<serde_json::Value> = fs::read_to_string(events)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for name in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        assert!(
            events.iter().any(|event| event["hook_event_name"] == name),
            "missing {name}"
        );
    }
    let id = events
        .iter()
        .find(|event| event["hook_event_name"] == "UserPromptSubmit")
        .unwrap()["session_id"]
        .as_str()
        .unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert!(
        events
            .iter()
            .filter(|event| event["hook_event_name"] == "Stop")
            .any(
                |event| event["last_assistant_message"].as_str().is_some_and(
                    |message| message.contains("echo: Pika offline compatibility fixture")
                )
            )
    );
}
