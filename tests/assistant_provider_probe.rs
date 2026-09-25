//! Opt-in installed-provider compatibility probes. No login or model turn.
//! The turn-shape probe replaces every turn target with a nonexistent UUID
//! before it reaches the real server, so validation cannot spend model quota.
#![cfg(unix)]
use pikamux::assistant_provider::{
    MainAssistant, MainProfile, ProviderError, RpcTransport, ServerEvent,
};
use pikamux::assistant_transport::{CodexTransport, TransportConfig};
use serde_json::json;

#[test]
#[ignore = "requires explicitly selected local Codex binary; never a normal test dependency"]
fn isolated_installed_protocol_without_login_or_model_turn() {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::path::PathBuf::from(
        std::env::var_os("PIKA_PROTOCOL_PROBE_BINARY").expect("explicit binary required"),
    )
    .canonicalize()
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let codex_home = temporary.path().join("provider");
    let scratch = temporary.path().join("scratch");
    for path in [&codex_home, &scratch] {
        std::fs::create_dir(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut transport = CodexTransport::spawn(TransportConfig {
        executable,
        codex_home: codex_home.clone(),
        scratch,
    })
    .unwrap();
    transport.request("initialize", json!({"clientInfo":{"name":"pika_protocol_probe","version":"0.1"},"capabilities":{"experimentalApi":true}})).unwrap();
    transport.notify("initialized", json!({})).unwrap();
    assert!(!codex_home.join("auth.json").exists());
    // Never call thread/start, thread/resume or turn/start in this probe.
}

/// Exercise production serialization, but make a model turn impossible.
struct NoModelProbe {
    inner: CodexTransport,
    rejected_turn: Option<String>,
}

impl RpcTransport for NoModelProbe {
    fn request(
        &mut self,
        method: &str,
        mut params: serde_json::Value,
    ) -> Result<serde_json::Value, ProviderError> {
        assert!(matches!(
            method,
            "initialize" | "thread/start" | "turn/start"
        ));
        if method == "turn/start" {
            assert_eq!(params["permissions"], "pika-assistant");
            assert!(params.get("sandboxPolicy").is_none());
            // Never forward the actual thread created by the adapter.
            params["threadId"] = json!("00000000-0000-4000-8000-000000000000");
            let reply = self.inner.request(method, params);
            self.rejected_turn = Some(format!("{:?}", reply.as_ref().err()));
            assert!(reply.is_err(), "nonexistent target unexpectedly accepted");
            return reply;
        }
        self.inner.request(method, params)
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), ProviderError> {
        assert_eq!(method, "initialized");
        self.inner.notify(method, params)
    }

    fn notifications(&mut self) -> Result<Vec<ServerEvent>, ProviderError> {
        panic!("no model turn exists to poll")
    }

    fn interrupt(&mut self, _: &str, _: &str) -> Result<(), ProviderError> {
        panic!("no model turn exists to interrupt")
    }
}

#[test]
#[ignore = "requires explicitly selected local Codex binary; never a normal test dependency"]
fn installed_adapter_permissions_and_turn_shape_without_model_dispatch() {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::path::PathBuf::from(
        std::env::var_os("PIKA_PROTOCOL_PROBE_BINARY").expect("explicit binary required"),
    )
    .canonicalize()
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let codex_home = temporary.path().join("provider");
    let scratch = temporary.path().join("scratch");
    for path in [&codex_home, &scratch] {
        std::fs::create_dir(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let transport = CodexTransport::spawn(TransportConfig {
        executable,
        codex_home: codex_home.clone(),
        scratch,
    })
    .unwrap();
    let mut assistant = MainAssistant::new(
        NoModelProbe {
            inner: transport,
            rejected_turn: None,
        },
        MainProfile {
            profile_id: "protocol-only".into(),
            thread_id: None,
        },
    );
    assistant.start_or_resume().unwrap();
    assert!(
        assistant
            .begin_turn("No model call: target is replaced by the probe")
            .is_err()
    );
    let rejection = assistant.transport().rejected_turn.as_deref().unwrap();
    assert!(rejection.contains("thread not found"), "{rejection}");
    assert!(!rejection.contains("no longer supported"), "{rejection}");
    assert!(!codex_home.join("auth.json").exists());
}
