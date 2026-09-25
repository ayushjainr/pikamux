//! Explicit foreground investigation intents; views do not own workers.
use crate::{
    assistant_investigation_service::{InvestigationService, InvestigationServiceConfig},
    assistant_memory::Scope,
    assistant_service::{AssistantService, ServiceState},
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
#[derive(Default)]
pub(crate) struct Investigations {
    config: Option<InvestigationServiceConfig>,
    service: Option<AssistantService>,
    hidden: bool,
}
impl Investigations {
    pub(crate) fn enable(&mut self, root: &Path, executable: PathBuf, scope: &str) {
        if scope == "personal" {
            self.config = Some(InvestigationServiceConfig {
                root: root.into(),
                executable,
                scope: Scope {
                    project: Some(scope.into()),
                    ..Scope::default()
                },
            });
        }
    }
    pub(crate) fn busy(&self) -> bool {
        self.service.as_ref().is_some_and(|s| s.busy())
    }
    pub(crate) fn begin(&mut self, scope: &str, id: &str, prompt: &str) -> Result<()> {
        if self.hidden || scope != "personal" {
            bail!("Investigation is not enabled in this scope");
        }
        if self.busy() {
            bail!("One investigation is already running");
        }
        if prompt.is_empty() || prompt.len() > 4096 {
            bail!("Use an investigation question of 1–4096 bytes");
        }
        if self.service.is_none() {
            let config = self.config.clone().ok_or_else(|| {
                anyhow::anyhow!("Enable the foreground provider and call allowance first")
            })?;
            self.service = Some(AssistantService::spawn(move || {
                InvestigationService::new(config)
            }));
        }
        self.service
            .as_ref()
            .unwrap()
            .begin(id, prompt)
            .map_err(|e| anyhow::anyhow!("Investigation not accepted: {e:?}"))
    }
    pub(crate) fn cancel(&self) -> Result<()> {
        if let Some(s) = &self.service {
            s.cancel().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        Ok(())
    }
    pub(crate) fn forget(&mut self) -> Result<()> {
        self.hidden = true;
        self.cancel()
    }
    pub(crate) fn snapshot(&self, scope: &str) -> Value {
        if self.hidden || scope != "personal" {
            return Value::Null;
        }
        self.service.as_ref().map(|s|{let snap=s.snapshot();let text=match snap.result {Some(crate::assistant_provider::TurnResult::Complete{text,..})=>text,_=>snap.partial};json!({"state":format!("{:?}",snap.state),"active":matches!(snap.state,ServiceState::Running|ServiceState::Starting|ServiceState::Cancelling),"text":text,"error":snap.error,"notice":"Up to two disposable workers plus synthesis; charged to the same foreground allowance. No project threads were opened."})}).unwrap_or(Value::Null)
    }
}
