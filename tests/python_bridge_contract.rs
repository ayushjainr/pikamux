#![cfg(unix)]

use pikamux::model::{ExpertProfile, FleetNode, Provider, Session, Status};
use pikamux::store::{Store, StoredExpertProfile};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
];

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn python_site(prefix: &Path) -> PathBuf {
    let output = Command::new(prefix.join("bin/python"))
        .args([
            "-c",
            "import site; print(next(p for p in site.getsitepackages() if p.endswith('site-packages')))",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn durable_session() -> Session {
    Session {
        provider: Provider::Codex,
        session_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        name: Some("strategy_dashboard".into()),
        cwd: Some("/work/strategy-dashboard".into()),
        branch: Some("main".into()),
        transcript_path: Some("/transcripts/strategy_dashboard.jsonl".into()),
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Ready,
        unread: true,
        model: Some("gpt-test".into()),
        source: "managed".into(),
        managed: true,
        error: None,
        attention_reason: Some("result ready".into()),
        created_at: 1.0,
        updated_at: 2.0,
        last_event_at: 2.0,
        last_activity_at: 2.0,
        live: false,
        attached: false,
        home_state: "missing".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

fn seed_durable_state(home: &Path) -> (PathBuf, String) {
    let state = home.join("state");
    let config = home.join("config/config.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let config_bytes = "{\n  \"version\": 1,\n  \"default_provider\": \"claude\",\n  \"future_setting\": {\"preserve\": true}\n}\n";
    fs::write(&config, config_bytes).unwrap();
    let store = Store::at(state.join("pika.db"));
    store.initialize().unwrap();
    let session = durable_session();
    store.upsert_session(&session, true).unwrap();
    store
        .put_expert_profile(&StoredExpertProfile {
            profile: ExpertProfile {
                provider: session.provider,
                session_id: session.session_id.clone(),
                summary: "Owns strategy dashboard state semantics".into(),
                current_state: "Ready for review".into(),
                topics: vec!["dashboard".into(), "strategy".into()],
                artifacts: vec!["README.md".into()],
                source: "interview".into(),
                updated_at: 3.0,
                scope_updated_at: 3.0,
                current_state_updated_at: 3.0,
            },
            transcript_mtime_ns: Some(10),
            transcript_size: Some(20),
            current_state_mtime_ns: Some(10),
            current_state_size: Some(20),
        })
        .unwrap();
    let node_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned();
    store
        .upsert_fleet_node(&FleetNode {
            node_id: node_id.clone(),
            alias: "research-node".into(),
            ssh_target: "research-node.example".into(),
            sources: vec!["ssh-config".into()],
            status: "ready".into(),
            protocol_version: Some(2),
            package_version: Some("0.5.0a4".into()),
            capabilities: vec!["snapshot".into()],
            last_seen: 1.0,
            last_attempt_at: 1.0,
            last_error: None,
            created_at: 1.0,
            updated_at: 1.0,
        })
        .unwrap();
    (config, node_id)
}

fn native_bundle(root: &Path) -> PathBuf {
    let version = env!("CARGO_PKG_VERSION");
    let binary = Path::new(env!("CARGO_BIN_EXE_pika"));
    let output = root.join("native-bundle");
    let mut command = Command::new("bash");
    command
        .arg("scripts/package-release.sh")
        .arg(version)
        .arg(&output)
        .env("PIKA_CROSS_PACKAGE", "1");
    for target in TARGETS {
        command.arg(format!("{target}={}", binary.display()));
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

fn bridge_bundle(root: &Path, native: &Path) -> PathBuf {
    let output = root.join("bridge-bundle");
    fs::create_dir(&output).unwrap();
    let wheel = output.join("pikamux-0.5.0a5-py3-none-any.whl");
    let mut command = Command::new("python3");
    command
        .arg("scripts/package-python-bridge.py")
        .args(["0.5.0a5", env!("CARGO_PKG_VERSION")])
        .arg(&wheel);
    for target in TARGETS {
        command.arg(format!(
            "{target}={}",
            native
                .join(format!(
                    "pikamux-{}-{target}.tar.gz",
                    env!("CARGO_PKG_VERSION")
                ))
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
    assert!(wheel.metadata().unwrap().len() < 64 * 1024 * 1024);
    output
}

#[test]
fn bridge_target_selection_is_exact_and_unsupported_hosts_fail_closed() {
    let script = r#"
from pikamux_bridge.cli import BridgeError, native_target
assert native_target("Darwin", "arm64") == "aarch64-apple-darwin"
assert native_target("Darwin", "x86_64") == "x86_64-apple-darwin"
assert native_target("Linux", "aarch64") == "aarch64-unknown-linux-musl"
assert native_target("Linux", "x86_64") == "x86_64-unknown-linux-musl"
try:
    native_target("Windows", "AMD64")
except BridgeError:
    pass
else:
    raise AssertionError("unsupported target was accepted")
"#;
    let output = Command::new("python3")
        .args(["-c", script])
        .env(
            "PYTHONPATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bridge"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bridge_rejects_duplicate_json_and_streams_native_artifacts_to_a_hard_limit() {
    let temp = tempfile::tempdir().unwrap();
    let script = r#"
from pathlib import Path
import sys
from pikamux_bridge.cli import MAX_NATIVE_ARCHIVE_BYTES, _sha256_bounded, _strict_json

root = Path(sys.argv[1])
duplicate = root / "duplicate.json"
duplicate.write_text('{"schema":1,"schema":2}\n')
try:
    _strict_json(duplicate)
except ValueError as error:
    assert "duplicate JSON key" in str(error)
else:
    raise AssertionError("duplicate JSON key was accepted")

oversized_json = root / "oversized.json"
oversized_json.write_bytes(b" " * (64 * 1024 + 1))
try:
    _strict_json(oversized_json)
except ValueError as error:
    assert "exceeds" in str(error)
else:
    raise AssertionError("oversized JSON was accepted")

archive = root / "archive.tar.gz"
with archive.open("wb") as stream:
    stream.seek(MAX_NATIVE_ARCHIVE_BYTES)
    stream.write(b"x")
try:
    _sha256_bounded(archive, limit=MAX_NATIVE_ARCHIVE_BYTES)
except ValueError as error:
    assert "exceeds" in str(error)
else:
    raise AssertionError("oversized native archive was accepted")
"#;
    let output = Command::new("python3")
        .args(["-c", script])
        .arg(temp.path())
        .env(
            "PYTHONPATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bridge"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bridge_cancels_and_reaps_its_process_group_and_reports_the_current_link() {
    let temp = tempfile::tempdir().unwrap();
    let script = r#"
from pathlib import Path
import signal
import subprocess
import sys
import pikamux_bridge.cli as cli

root = Path(sys.argv[1]) / "managed"
releases = root / "releases"
bridge = releases / "bridge"
native = releases / "native"
bundle = Path(sys.argv[1]) / "bundle"
bin_dir = Path(sys.argv[1]) / "bin"
for path in (bridge, native, bundle, bin_dir):
    path.mkdir(parents=True, exist_ok=True)
current = root / "current"

def point_to(path):
    current.unlink(missing_ok=True)
    current.symlink_to(path, target_is_directory=True)

point_to(bridge)
cli.sys.prefix = str(bridge)
cli._managed_receipt = lambda: (root, bin_dir)
cli._native_bundle = lambda: bundle
kills = []
cli.os.killpg = lambda pid, sig: kills.append((pid, sig))

class InterruptedProcess:
    pid = 4242
    def __init__(self):
        self.calls = 0
        self.reaped = False
    def poll(self):
        return -9 if self.reaped else None
    def wait(self, timeout=None):
        self.calls += 1
        if self.calls == 1:
            point_to(native)
            raise KeyboardInterrupt()
        if self.calls == 2:
            raise subprocess.TimeoutExpired("install", timeout)
        self.reaped = True
        return -9

interrupted = InterruptedProcess()
def interrupted_popen(_command, **options):
    assert options == {"start_new_session": True}
    return interrupted
cli.subprocess.Popen = interrupted_popen
try:
    cli._activate_and_exec([])
except cli.BridgeError as error:
    detail = str(error)
    assert "interrupted" in detail
    assert "managed release 'native' is now current" in detail
else:
    raise AssertionError("KeyboardInterrupt escaped bridge cleanup")
assert interrupted.reaped
assert kills == [(4242, signal.SIGTERM), (4242, signal.SIGKILL)]

class TimeoutProcess:
    pid = 4343
    def __init__(self): self.calls = 0
    def poll(self): return None if self.calls < 2 else -15
    def wait(self, timeout=None):
        self.calls += 1
        if self.calls == 1: raise subprocess.TimeoutExpired("install", timeout)
        return -15

point_to(bridge)
timed_out = TimeoutProcess()
cli.subprocess.Popen = lambda *_args, **_kwargs: timed_out
try:
    cli._activate_and_exec([])
except cli.BridgeError as error:
    assert "timed out; the Python bridge remains current" in str(error)
else:
    raise AssertionError("activation timeout was accepted")

class FailedProcess:
    pid = 4444
    def poll(self): return 7
    def wait(self, timeout=None):
        point_to(native)
        return 7

failed = FailedProcess()
cli.subprocess.Popen = lambda *_args, **_kwargs: failed
try:
    cli._activate_and_exec([])
except cli.BridgeError as error:
    assert "exited 7; managed release 'native' is now current" in str(error)
else:
    raise AssertionError("failed activation was accepted")
"#;
    let output = Command::new("python3")
        .args(["-c", script])
        .arg(temp.path())
        .env(
            "PYTHONPATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bridge"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unmodified_v050a4_default_update_discovers_bridge_then_first_use_execs_native() {
    let temp = tempfile::tempdir().unwrap();
    let native = native_bundle(temp.path());
    let bridge = bridge_bundle(temp.path(), &native);
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let old = root.join("releases/0.5.0a4-seed");
    let home = temp.path().join("home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&home).unwrap();
    let (config, node_id) = seed_durable_state(&home);
    let config_before = fs::read(&config).unwrap();
    fs::create_dir_all(root.join("tools")).unwrap();
    let created = Command::new("python3")
        .args(["-m", "venv", "--without-pip"])
        .arg(&old)
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let old_site = python_site(&old);
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python-v0.5.0a4/pikamux"),
        &old_site.join("pikamux"),
    );
    fs::write(
        old_site.join("sitecustomize.py"),
        r#"import io
import json
import os
from pathlib import Path
import urllib.request

class _OfflineRelease:
    def __init__(self, url, payload):
        self.url = url
        self._payload = io.BytesIO(payload)
    def __enter__(self): return self
    def __exit__(self, *_args): return False
    def read(self, size=-1): return self._payload.read(size)

def _offline_urlopen(url, timeout=None):
    release = Path(os.environ["PIKA_TEST_RELEASE_DIR"])
    trace = Path(os.environ["PIKA_TEST_RELEASE_TRACE"])
    with trace.open("a") as output:
        output.write(str(url) + "\n")
    if str(url).startswith("https://api.github.com/"):
        version = "0.5.0a5"
        payload = [{
            "draft": False,
            "prerelease": True,
            "tag_name": "v" + version,
            "assets": [
                {"name": "pika-release.json"},
                {"name": f"pikamux-{version}-py3-none-any.whl"},
            ],
        }]
        return _OfflineRelease(str(url), json.dumps(payload).encode())
    name = str(url).rsplit("/", 1)[-1]
    asset = release / name
    if not asset.is_file():
        raise OSError(f"offline fixture has no {name}")
    return _OfflineRelease(str(url), asset.read_bytes())

urllib.request.urlopen = _offline_urlopen
"#,
    )
    .unwrap();
    executable(
        &old.join("bin/pika"),
        "#!/bin/sh\nexec \"$(dirname \"$0\")/python\" -m pikamux \"$@\"\n",
    );
    fs::write(root.join(".pika-install-root"), "pikamux-installer-v1\n").unwrap();
    fs::write(
        old.join(".pika-install.json"),
        serde_json::json!({
            "schema": 1,
            "root": root.canonicalize().unwrap(),
            "bin_dir": bin.canonicalize().unwrap(),
            "version": "0.5.0a4",
            "sha256": "0".repeat(64),
        })
        .to_string(),
    )
    .unwrap();
    symlink(&old, root.join("current")).unwrap();
    symlink(
        root.canonicalize().unwrap().join("current/bin/pika"),
        bin.join("pika"),
    )
    .unwrap();
    executable(
        &root.join("tools/uv"),
        r#"#!/bin/sh
set -eu
[ "$1" = "--no-config" ]
shift
case "$1" in
  venv)
    shift
    [ "$1" = "--python" ]
    "$2" -m venv "$3"
    ;;
  pip)
    shift
    [ "$1" = "install" ]
    shift
    [ "$1" = "--python" ]
    python=$2
    shift 2
    for argument in "$@"; do wheel=$argument; done
    "$python" -m pip install --disable-pip-version-check --no-index "$wheel"
    ;;
  *) exit 64 ;;
esac
"#,
    );

    let release_trace = temp.path().join("release-requests");
    let update = Command::new(old.join("bin/pika"))
        .arg("update")
        .env("HOME", &home)
        .env("PIKA_TEST_RELEASE_DIR", &bridge)
        .env("PIKA_TEST_RELEASE_TRACE", &release_trace)
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}{}",
        String::from_utf8_lossy(&update.stdout),
        String::from_utf8_lossy(&update.stderr)
    );
    let requests = fs::read_to_string(&release_trace).unwrap();
    let requests: Vec<_> = requests.lines().collect();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(requests[0].starts_with("https://api.github.com/"));
    assert!(requests[1].ends_with("/v0.5.0a5/pika-release.json"));
    assert!(requests[2].ends_with("/v0.5.0a5/pikamux-0.5.0a5-py3-none-any.whl"));
    let bridge_prefix = root.join("current").canonicalize().unwrap();
    assert_ne!(bridge_prefix, old.canonicalize().unwrap());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(bridge_prefix.join(".pika-install.json")).unwrap()
        )
        .unwrap()["version"],
        "0.5.0a5"
    );

    for arguments in [["--version"].as_slice(), ["skill", "show"].as_slice()] {
        let probe = Command::new(bin.join("pika"))
            .args(arguments)
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(probe.status.success());
        assert_eq!(root.join("current").canonicalize().unwrap(), bridge_prefix);
    }

    let first_use = Command::new(bin.join("pika"))
        .args(["machines", "list", "--json"])
        .env("HOME", &home)
        .env("PIKA_CONFIG_HOME", home.join("config"))
        .env("PIKA_STATE_HOME", home.join("state"))
        .env("CODEX_HOME", home.join("codex"))
        .output()
        .unwrap();
    assert!(
        first_use.status.success(),
        "{}{}",
        String::from_utf8_lossy(&first_use.stdout),
        String::from_utf8_lossy(&first_use.stderr)
    );
    let native_prefix = root.join("current").canonicalize().unwrap();
    assert_ne!(native_prefix, bridge_prefix);
    assert_eq!(fs::read(&config).unwrap(), config_before);
    let migrated = Store::at(home.join("state/pika.db"));
    let session = migrated
        .get_session(Provider::Codex, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .unwrap()
        .unwrap();
    assert_eq!(session.name.as_deref(), Some("strategy_dashboard"));
    assert_eq!(session.status, Status::Ready);
    assert!(session.unread);
    let card = migrated
        .get_stored_expert_profile(Provider::Codex, &session.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(card.profile.topics, ["dashboard", "strategy"]);
    assert_eq!(
        migrated.get_fleet_node(&node_id).unwrap().unwrap().alias,
        "research-node"
    );
    let version = Command::new(bin.join("pika"))
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!("pika {}", env!("CARGO_PKG_VERSION"))
    );
}
