#![cfg(unix)]

use pikamux::{
    config::Config,
    core::Pika,
    model::Provider,
    paths::Paths,
    process,
    store::{LaunchPhase, Store},
    tmux::{Tmux, WINDOWS_TERMINAL_DA2_RESPONSE},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

struct IsolatedTmux(String);

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .status();
    }
}

fn raw_tmux(socket: &str, arguments: &[&str]) -> std::process::Output {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(arguments)
        .output()
        .unwrap()
}

fn isolated() -> (IsolatedTmux, Tmux) {
    let socket = format!("pika-identity-test-{}", uuid::Uuid::new_v4());
    (
        IsolatedTmux(socket.clone()),
        Tmux::with_executable("tmux", Some(socket)),
    )
}

fn paths(root: &Path) -> Paths {
    Paths {
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        config: root.join("config/config.json"),
        database: root.join("state/pika.db"),
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_data_home: root.join("opencode-data"),
        opencode_config_home: root.join("opencode-config"),
    }
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn guarded_tag_removal_rejects_a_replaced_pane_generation() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let (guard, tmux) = isolated();
    assert!(
        raw_tmux(
            &guard.0,
            &["new-session", "-d", "-s", "pika-c-safety", "sleep 30"]
        )
        .status
        .success()
    );
    let original = tmux.get_pane("pika-c-safety").unwrap().unwrap();
    tmux.tag_pane_if_unchanged(
        &original,
        Some(Provider::Codex),
        Some("11111111-1111-4111-8111-111111111111"),
        Some("safety"),
        Some("launch-a"),
    )
    .unwrap();
    let bound = tmux.get_pane(&original.pane_id).unwrap().unwrap();

    assert!(
        raw_tmux(
            &guard.0,
            &["respawn-pane", "-k", "-t", &bound.pane_id, "sleep 30"]
        )
        .status
        .success()
    );
    assert!(tmux.clear_tags_if_unchanged(&bound).is_err());
    let replacement = tmux.get_pane(&bound.pane_id).unwrap().unwrap();
    assert_ne!(replacement.pane_pid, bound.pane_pid);
    assert_eq!(replacement.pika_provider, Some(Provider::Codex));
    assert_eq!(
        replacement.pika_session_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );
}

#[test]
fn terminal_guard_never_overwrites_an_existing_user_key_binding() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let (guard, tmux) = isolated();
    assert!(
        raw_tmux(&guard.0, &["new-session", "-d", "-s", "keys", "sleep 30"])
            .status
            .success()
    );
    assert!(
        raw_tmux(
            &guard.0,
            &[
                "set-option",
                "-s",
                "user-keys[500]",
                WINDOWS_TERMINAL_DA2_RESPONSE,
            ]
        )
        .status
        .success()
    );
    assert!(
        raw_tmux(
            &guard.0,
            &[
                "bind-key",
                "-T",
                "root",
                "User500",
                "display-message",
                "user-owned",
            ]
        )
        .status
        .success()
    );
    assert!(
        raw_tmux(
            &guard.0,
            &["set-option", "-s", "user-keys[501]", "reserved-by-user"]
        )
        .status
        .success()
    );
    assert!(tmux.ensure_terminal_reply_guard());
    let keys = String::from_utf8_lossy(&raw_tmux(&guard.0, &["list-keys", "-T", "root"]).stdout)
        .into_owned();
    let occupied = keys
        .lines()
        .find(|line| line.split_whitespace().any(|field| field == "User500"))
        .unwrap();
    assert!(occupied.contains("user-owned"));
    let marker = String::from_utf8_lossy(
        &raw_tmux(
            &guard.0,
            &["show-options", "-s", "-v", "@pika_terminal_reply_key"],
        )
        .stdout,
    )
    .trim()
    .to_owned();
    assert_eq!(marker, "502");
    let reserved = String::from_utf8_lossy(
        &raw_tmux(&guard.0, &["show-options", "-s", "-v", "user-keys[501]"]).stdout,
    )
    .trim()
    .to_owned();
    assert_eq!(reserved, "reserved-by-user");
}

fn launch_fixture(temp: &tempfile::TempDir, socket: &str, tmux_executable: PathBuf) -> Pika {
    let fake_provider = temp.path().join("claude");
    executable(
        &fake_provider,
        "#!/bin/sh\nprintf 'fake claude ready\\n'\nwhile :; do sleep 1; done\n",
    );
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    let config = Config {
        provider_executables: BTreeMap::from([(
            "claude".to_owned(),
            fake_provider.to_string_lossy().into_owned(),
        )]),
        alerts: "none".into(),
        ..Config::default()
    };
    Pika::with_components(
        paths,
        config,
        store,
        Tmux::with_executable(tmux_executable.to_string_lossy(), Some(socket.to_owned())),
    )
}

#[test]
fn tag_failure_occurs_before_provider_execution_and_keeps_recovery_record() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-tag-failure-{}", uuid::Uuid::new_v4());
    let _guard = IsolatedTmux(socket.clone());
    let wrapper = temp.path().join("tmux-fail-tags");
    executable(
        &wrapper,
        "#!/bin/sh\ncase \" $* \" in *\" if-shell \"*) exit 44;; esac\nexec tmux \"$@\"\n",
    );
    let pika = launch_fixture(&temp, &socket, wrapper);
    assert!(
        pika.new_session("tag_failure", Provider::Claude, false)
            .is_err()
    );
    let pending = pika.store.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pika.store
            .get_launch_phase(&pending[0].launch_token)
            .unwrap(),
        Some(LaunchPhase::PaneAllocated)
    );
    assert!(pending[0].tmux_pane.is_some());
    let panes = Tmux::with_executable("tmux", Some(socket.clone()))
        .list_panes()
        .unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].current_command, "sleep");
    let observed = process::observe();
    let processes = observed.require_complete("test launch failure").unwrap();
    assert!(
        process::process_tree(panes[0].pane_pid, processes)
            .into_iter()
            .all(|pid| processes
                .get(&pid)
                .and_then(|record| record.provider())
                .is_none())
    );
}

#[test]
fn pane_replacement_between_readback_and_respawn_never_starts_provider() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-respawn-race-{}", uuid::Uuid::new_v4());
    let _guard = IsolatedTmux(socket.clone());
    let wrapper = temp.path().join("tmux-replace-before-respawn");
    executable(
        &wrapper,
        &format!(
            "#!/bin/sh\ncase \"$*\" in *if-shell*respawn-pane*) pane=$(tmux -L '{}' list-panes -a -F '#{{pane_id}}' | head -n 1); tmux -L '{}' respawn-pane -k -t \"$pane\" 'sleep 30';; esac\nexec tmux \"$@\"\n",
            socket, socket
        ),
    );
    let pika = launch_fixture(&temp, &socket, wrapper);
    assert!(
        pika.new_session("respawn_race", Provider::Claude, false)
            .is_err()
    );
    let pending = pika.store.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pika.store
            .get_launch_phase(&pending[0].launch_token)
            .unwrap(),
        Some(LaunchPhase::ProviderStarting)
    );
    let panes = Tmux::with_executable("tmux", Some(socket.clone()))
        .list_panes()
        .unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].current_command, "sleep");
    let observed = process::observe();
    let processes = observed.require_complete("test respawn race").unwrap();
    assert!(
        process::process_tree(panes[0].pane_pid, processes)
            .into_iter()
            .all(|pid| processes
                .get(&pid)
                .and_then(|record| record.provider())
                .is_none())
    );
}

#[test]
fn post_execution_readback_failure_retains_exact_pending_generation() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-readback-failure-{}", uuid::Uuid::new_v4());
    let _guard = IsolatedTmux(socket.clone());
    let marker = temp.path().join("provider-started");
    let wrapper = temp.path().join("tmux-fail-readback");
    let real_tmux = String::from_utf8_lossy(
        &Command::new("sh")
            .args(["-c", "command -v tmux"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_owned();
    executable(
        &wrapper,
        &format!(
            "#!/bin/sh\nmarker={}\nif [ \"$3\" = list-panes ] && [ -f \"$marker\" ]; then exit 45; fi\nif [ \"$3\" = if-shell ] && printf '%s' \"$*\" | grep -q 'respawn-pane'; then\n  {} \"$@\"\n  rc=$?\n  : > \"$marker\"\n  exit $rc\nfi\nexec {} \"$@\"\n",
            shell_words::quote(&marker.to_string_lossy()),
            shell_words::quote(&real_tmux),
            shell_words::quote(&real_tmux),
        ),
    );
    let pika = launch_fixture(&temp, &socket, wrapper);
    assert!(
        pika.new_session("readback_failure", Provider::Claude, false)
            .is_err()
    );
    let pending = pika.store.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].tmux_pane.is_some());
    assert!(pending[0].root_pid.is_some());
    assert_eq!(
        pika.store
            .get_launch_phase(&pending[0].launch_token)
            .unwrap(),
        Some(LaunchPhase::ProviderStarting)
    );
    let raw = Tmux::with_executable("tmux", Some(socket))
        .list_panes()
        .unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].pika_provider, Some(Provider::Claude));
    assert_eq!(
        raw[0].pika_launch_token.as_deref(),
        Some(pending[0].launch_token.as_str())
    );
}
