//! Disposable, bounded authoring assistant construction.
//!
//! Authoring is deliberately a separate provider context: it shares the
//! durable human memory and policy ledger, but never resumes the foreground
//! assistant journal or its provider thread.  The host must still explicitly
//! configure allowance in the shared policy before a turn can spend it.

use crate::assistant_memory::{Scope, Store};
use crate::assistant_policy::AssistantPolicy;
use crate::assistant_provider::{MainAssistant, MainProfile};
use crate::assistant_runtime::AssistantRuntime;
use crate::assistant_service::AssistantService;
use crate::assistant_transport::{CodexTransport, TransportConfig};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AuthorConfig {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub scope: Scope,
}

/// Create one disposable authoring worker.
///
/// The memory and policy databases are shared with the main assistant.  The
/// author journal is separate and each provider scratch directory is unique, so a crash or
/// unknown delivery in an authoring turn cannot be mistaken for a main turn.
pub fn spawn(config: AuthorConfig) -> Result<AssistantService, String> {
    validate_config(&config)?;
    crate::assistant_storage::directory(&config.root).map_err(|e| e.to_string())?;

    let memory_path = config.root.join("memory.sqlite");
    let policy_path = config.root.join("policy.sqlite");
    let provider_home = config.root.join("provider-home");
    crate::assistant_storage::directory(&provider_home).map_err(|e| e.to_string())?;

    let scratch = config
        .root
        .join("author-scratch")
        .join(uuid::Uuid::new_v4().to_string());
    crate::assistant_storage::directory(&scratch).map_err(|e| e.to_string())?;

    let transport_config = TransportConfig {
        executable: config.executable.clone(),
        codex_home: provider_home,
        scratch,
    };
    transport_config.validate().map_err(|e| e.to_string())?;

    let root = config.root;
    let scope = config.scope;
    Ok(AssistantService::spawn(move || {
        let memory = Store::open(&memory_path).map_err(|e| e.to_string())?;
        let profile_id = memory.profile_id().to_owned();
        let policy = AssistantPolicy::open(&policy_path).map_err(|e| e.to_string())?;
        let provider = MainAssistant::new(
            CodexTransport::spawn(transport_config).map_err(|e| e.to_string())?,
            MainProfile {
                profile_id,
                thread_id: None,
            },
        );
        let journal = root.join("author-runtime.sqlite");
        let mut runtime = AssistantRuntime::open(provider, memory, policy, journal, scope)
            .map_err(|e| e.to_string())?;
        runtime
            .start_fresh_disposable(now())
            .map_err(|e| e.to_string())?;
        Ok(runtime)
    }))
}

fn validate_config(config: &AuthorConfig) -> Result<(), String> {
    if !config.root.is_absolute() {
        return Err("author root must be absolute".into());
    }
    if !config.executable.is_absolute() {
        return Err("provider executable must be absolute".into());
    }
    if config.scope.project.as_deref() != Some("personal")
        || config.scope.node.is_some()
        || config.scope.provider.is_some()
        || config.scope.conversation.is_some()
    {
        return Err("author scope must be the personal scope".into());
    }
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_personal_or_non_absolute_author_config() {
        let invalid_root = AuthorConfig {
            root: PathBuf::from("relative"),
            executable: PathBuf::from("/bin/true"),
            scope: Scope {
                project: Some("personal".into()),
                ..Scope::default()
            },
        };
        assert!(validate_config(&invalid_root).is_err());

        let invalid_scope = AuthorConfig {
            root: PathBuf::from("/tmp/author"),
            executable: PathBuf::from("/bin/true"),
            scope: Scope {
                project: Some("project-a".into()),
                ..Scope::default()
            },
        };
        assert!(validate_config(&invalid_scope).is_err());
    }
}
