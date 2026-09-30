//! Foreground host integration. Views submit intent; one worker owns the model.
use crate::{
    assistant_memory::{Scope, Store},
    assistant_policy::AssistantPolicy,
    assistant_provider::{MainAssistant, MainProfile, TurnResult},
    assistant_runtime::{AssistantRuntime, RuntimeConfig},
    assistant_service::{AssistantService, ServiceState},
    assistant_transport::{CodexTransport, TransportConfig},
    assistant_workshop_ui::WorkshopUi,
};
use anyhow::{Result, bail};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::Digest;
use std::path::{Path, PathBuf};

use crate::assistant_evolution::{
    CandidateManifest, EvaluationReport, Scope as ToolScope, ScopedInput, ToolResult,
    validate_definition,
};

const MAX_EVOLUTION_INPUT_BYTES: usize = 32 * 1024;

struct PendingEvolution {
    request_id: String,
    scope: String,
    experiment: String,
    submitted: bool,
    terminal: bool,
    candidate_json: Option<String>,
    evaluation_queued: bool,
}

pub(crate) struct Session {
    service: Option<AssistantService>,
    author: Option<AssistantService>,
    author_config: Option<crate::assistant_author::AuthorConfig>,
    scope: Option<String>,
    redacted: bool,
    workshop: Option<WorkshopUi>,
    workshop_root: Option<PathBuf>,
    pending_evolution: Option<PendingEvolution>,
}
impl Session {
    pub(crate) fn tool_catalog(&mut self, scope: &str, hash: Option<&str>) -> Result<Value> {
        self.require_scope(scope)?;
        Ok(self
            .ensure_workshop_stored()?
            .catalog(&ToolScope::new([scope]), hash)?)
    }

    pub(crate) fn assess_evolution(
        &mut self,
        scope: &str,
        hash: &str,
        outcome: &str,
        evidence: &str,
        rollback: Option<&str>,
    ) -> Result<Value> {
        self.require_scope(scope)?;
        Ok(self.ensure_workshop_stored()?.assess(
            &ToolScope::new([scope]),
            hash,
            outcome,
            evidence,
            rollback,
        )?)
    }

    pub(crate) fn prepare_improvement(
        &self,
        root: &Path,
        scope: &str,
        correction_id: &str,
    ) -> Result<Value> {
        self.require_scope(scope)?;
        let record =
            crate::assistant_learning::prepare_correction_proposal(root, scope, correction_id)?;
        Ok(
            json!({"proposal":record,"sent_to_provider":false,"notice":"Scoped hypothesis saved, not an accepted decision. No model call, test, tool or activation occurred. Review the hypothesis and provide protected contrasting cases with /evolve-json to authorize an experiment."}),
        )
    }

    pub(crate) fn busy(&self) -> bool {
        self.service.as_ref().is_some_and(|service| service.busy())
            || self.author.as_ref().is_some_and(|service| service.busy())
    }
    pub(crate) fn new() -> Self {
        Self {
            service: None,
            author: None,
            author_config: None,
            scope: None,
            redacted: false,
            workshop: None,
            workshop_root: None,
            pending_evolution: None,
        }
    }

    pub(crate) fn set_workshop_root(&mut self, root: &Path) -> Result<()> {
        self.workshop_root = Some(root.to_path_buf());
        if self.redacted {
            return Ok(());
        }
        if let Some((request_id, scope, experiment, candidate_json, state)) = load_pending(root)? {
            self.pending_evolution = Some(PendingEvolution {
                request_id,
                scope,
                experiment,
                submitted: state == "submitted",
                terminal: false,
                candidate_json,
                evaluation_queued: false,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_service_for_test(service: AssistantService, scope: &str) -> Self {
        Self {
            service: None,
            author: Some(service),
            author_config: None,
            scope: Some(scope.into()),
            redacted: false,
            workshop: None,
            workshop_root: None,
            pending_evolution: None,
        }
    }

    #[cfg(test)]
    fn with_workshop_for_test(scope: &str, root: &Path) -> Self {
        Self {
            service: None,
            author: None,
            author_config: None,
            scope: Some(scope.into()),
            redacted: false,
            workshop: None,
            workshop_root: Some(root.to_path_buf()),
            pending_evolution: None,
        }
    }
    /// Explicit per-foreground-session permission. Never starts from a saved
    /// preference or a model-authored instruction. Lifetime spending remains
    /// charged in the separate durable policy ledger across process restarts.
    pub(crate) fn enable(
        &mut self,
        root: &Path,
        name: &str,
        executable: PathBuf,
        calls: u64,
    ) -> Result<()> {
        self.enable_with_background(root, name, executable, calls, 0)
    }

    pub(crate) fn enable_with_background(
        &mut self,
        root: &Path,
        name: &str,
        executable: PathBuf,
        calls: u64,
        background_calls: u64,
    ) -> Result<()> {
        validate_enable(self, root, calls)?;
        if background_calls > calls {
            bail!("Background calls cannot exceed the approved lifetime allowance");
        }
        let config = transport_config(root, &executable)?;
        let scope = project_scope(name);
        // Validation permits replacement only after initialization failed before
        // any request was accepted. Reap that owned worker; never replay a turn.
        self.service.take();
        self.author_config = Some(crate::assistant_author::AuthorConfig {
            root: root.to_path_buf(),
            executable,
            scope: scope.clone(),
        });
        self.service = Some(spawn_main_service(
            root.to_path_buf(),
            config,
            scope,
            calls,
            background_calls,
        ));
        self.scope = Some(name.into());
        self.workshop_root = Some(root.to_path_buf());
        Ok(())
    }
    pub(crate) fn permission(&self) -> Option<(Scope, PathBuf)> {
        if self.redacted {
            return None;
        }
        self.author_config
            .as_ref()
            .map(|config| (config.scope.clone(), config.executable.clone()))
    }

    pub(crate) fn begin_background(&self, name: &str, id: &str, prompt: &str) -> Result<()> {
        if self.redacted || self.scope.as_deref() != Some(name) || self.busy() {
            bail!("Background request is not currently eligible");
        }
        self.service
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Provider not enabled"))?
            .begin_background(id, prompt)
            .map_err(|error| anyhow::anyhow!("Background turn not accepted: {error:?}"))
    }
    #[cfg(test)]
    pub(crate) fn begin(&self, name: &str, id: &str, prompt: &str) -> Result<()> {
        if self.author.as_ref().is_some_and(|author| author.busy()) {
            bail!("Wait for or cancel the disposable tool author");
        }
        if self.redacted {
            bail!("Memory was forgotten; this provider context is blocked from reuse");
        }
        if self.scope.as_deref() != Some(name) {
            bail!("This scope has no enabled provider; your draft was not sent");
        }
        self.service
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Provider not enabled"))?
            .begin(id, prompt)
            .map_err(|error| anyhow::anyhow!("Turn not accepted: {error:?}"))
    }
    pub(crate) fn begin_user(
        &self,
        name: &str,
        id: &str,
        raw_body: &str,
        prompt: &str,
        timestamp: i64,
    ) -> Result<()> {
        if self.author.as_ref().is_some_and(|author| author.busy()) {
            bail!("Wait for or cancel the disposable tool author");
        }
        if self.redacted {
            bail!("Memory was forgotten; this provider context is blocked from reuse");
        }
        if self.scope.as_deref() != Some(name) {
            bail!("This scope has no enabled provider; your draft was not sent");
        }
        let scope = self
            .author_config
            .as_ref()
            .map(|config| config.scope.clone())
            .unwrap_or_else(|| Scope {
                project: Some(name.into()),
                ..Scope::default()
            });
        self.service
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Provider not enabled"))?
            .begin_user(
                id,
                crate::assistant_service::UserTurnInput {
                    raw_body: raw_body.into(),
                    prompt: prompt.into(),
                    scope,
                    timestamp,
                },
            )
            .map_err(|error| anyhow::anyhow!("Turn not accepted: {error:?}"))
    }
    pub(crate) fn cancel(&self) -> Result<()> {
        if let Some(author) = &self.author {
            author
                .cancel()
                .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        }
        if let Some(service) = &self.service {
            service
                .cancel()
                .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        }
        Ok(())
    }
    pub(crate) fn forget(&mut self) -> Result<()> {
        self.redacted = true;
        if let Some(pending) = self.pending_evolution.as_mut() {
            pending.terminal = true;
            if let Some(root) = self.workshop_root.as_ref() {
                let _ = mark_pending_terminal(root, &pending.request_id, "forgotten");
            }
        }
        self.pending_evolution = None;
        self.workshop.take();
        self.cancel()
    }
    /// Queue a bounded, host-scoped evolution request through the same
    /// foreground service and durable assistant allowance as ordinary turns.
    #[cfg(not(test))]
    pub(crate) fn evolve(&mut self, _root: &Path, _scope: &str, _request_id: &str) -> Result<()> {
        bail!(
            "Use /evolve CORRECTION_ID to prepare a scoped hypothesis without a model call, or /evolve-json with a reviewed need and protected contrasting cases to authorize an experiment"
        )
    }

    #[cfg(test)]
    pub(crate) fn evolve(&mut self, root: &Path, scope: &str, request_id: &str) -> Result<()> {
        let assigned = ToolScope::new([scope.to_owned()]);
        let spec = json!({
            "need": "filter records where status equals needs_you, then map title",
            "cases": [
                {"inputs": [{"value": {"status":"needs_you","title":"First"}, "scope": assigned}, {"value": {"status":"done","title":"Skip"}, "scope": ToolScope::new([scope.to_owned()])}], "expected": ["First"]},
                {"inputs": [{"value": {"status":"done","title":"Only skip"}, "scope": ToolScope::new([scope.to_owned()])}], "expected": []}
            ]
        });
        self.evolve_spec(root, scope, request_id, &spec.to_string())
    }

    pub(crate) fn evolve_spec(
        &mut self,
        root: &Path,
        scope: &str,
        request_id: &str,
        spec_json: &str,
    ) -> Result<()> {
        self.prepare_evolution(root, scope, request_id, spec_json)?;
        let spec = parse_evolution_spec(spec_json)?;
        validate_evolution_scope(&spec, scope)?;
        let experiment = experiment_id(scope, &spec.cases)?;
        persist_evolution_cases(self, root, scope, request_id, &experiment, &spec)?;
        let prompt = evolution_prompt(scope, &spec.need);
        self.dispatch_evolution_author(root, scope, request_id, experiment, prompt)
    }

    fn prepare_evolution(
        &mut self,
        root: &Path,
        scope: &str,
        request_id: &str,
        spec_json: &str,
    ) -> Result<()> {
        if self.redacted {
            bail!("Memory was forgotten; evolution is blocked")
        }
        // A completed evaluator turn may have become visible between host
        // calls.  Settle that receipt before admitting a new request so the
        // previous request cannot strand the session in `pending`.
        self.tick()?;
        if self.busy() {
            bail!("Wait for or cancel the current assistant work");
        }
        if self.service.as_ref().is_some_and(|service| {
            !matches!(
                service.snapshot().state,
                ServiceState::Idle | ServiceState::Completed
            )
        }) {
            bail!("The main provider must be ready before starting an author");
        }
        if self.scope.as_deref() != Some(scope) {
            bail!("This scope has no enabled provider")
        }
        if request_id.is_empty() || request_id.len() > 256 {
            bail!("bounded evolution request id required")
        }
        if self
            .pending_evolution
            .as_ref()
            .is_some_and(|pending| !pending.terminal)
        {
            bail!("an evolution request is already pending")
        }
        self.pending_evolution = None;
        if self.workshop.is_none() {
            self.workshop = Some(
                WorkshopUi::open(root.join("workshop.sqlite"))
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?,
            );
        }
        self.workshop_root = Some(root.to_path_buf());
        if spec_json.len() > 16 * 1024 {
            bail!("evolution specification exceeds 16 KiB")
        }
        Ok(())
    }

    fn dispatch_evolution_author(
        &mut self,
        root: &Path,
        scope: &str,
        request_id: &str,
        experiment: String,
        prompt: String,
    ) -> Result<()> {
        if let Some(config) = self.author_config.clone() {
            self.author = match crate::assistant_author::spawn(config) {
                Ok(author) => Some(author),
                Err(error) => {
                    mark_pending_terminal(root, request_id, "author_start_failed")?;
                    bail!("Disposable author could not start: {error}");
                }
            };
        }
        if let Err(error) = self
            .author
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Disposable author not enabled"))?
            .begin(request_id, &prompt)
        {
            let _ = mark_pending_terminal(root, request_id, "dispatch_failed");
            return Err(anyhow::anyhow!("Evolution turn not accepted: {error:?}"));
        }
        self.pending_evolution = Some(PendingEvolution {
            request_id: request_id.into(),
            scope: scope.into(),
            experiment,
            submitted: false,
            terminal: false,
            candidate_json: None,
            evaluation_queued: false,
        });
        Ok(())
    }

    pub(crate) fn approve_evolution(&mut self, scope: &str, hash: &str) -> Result<String> {
        self.require_scope(scope)?;
        let workshop = self.ensure_workshop_stored()?;
        let tool_scope = ToolScope::new([scope.to_owned()]);
        if hash.is_empty() || hash.len() > 128 {
            bail!("invalid exact candidate hash")
        }
        let grant = workshop
            .approve_exact(hash, tool_scope)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(grant)
    }

    pub(crate) fn revoke_evolution(&mut self, scope: &str, grant_id: &str) -> Result<()> {
        self.require_scope(scope)?;
        self.ensure_workshop_stored()?
            .revoke(grant_id)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }

    pub(crate) fn rollback_evolution(
        &mut self,
        scope: &str,
        name: &str,
        hash: &str,
    ) -> Result<String> {
        self.require_scope(scope)?;
        let tool_scope = ToolScope::new([scope.to_owned()]);
        self.ensure_workshop_stored()?
            .rollback_exact(name, hash, tool_scope)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }

    pub(crate) fn invoke_evolution(
        &mut self,
        scope: &str,
        name: &str,
        inputs_json: &str,
    ) -> Result<ToolResult> {
        self.require_scope(scope)?;
        if inputs_json.len() > MAX_EVOLUTION_INPUT_BYTES {
            bail!("evolution input exceeds 32 KiB")
        }
        let values: Vec<Value> = serde_json::from_str(inputs_json)
            .map_err(|error| anyhow::anyhow!("invalid pure input JSON: {error}"))?;
        let tool_scope = ToolScope::new([scope.to_owned()]);
        let inputs = values
            .into_iter()
            .map(|value| ScopedInput {
                value,
                scope: tool_scope.clone(),
            })
            .collect::<Vec<_>>();
        let mut result = self
            .ensure_workshop_stored()?
            .invoke(name, &inputs)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if let Some(root) = self.workshop_root.as_ref() {
            if let Err(error) = crate::assistant_learning::record_use(
                root,
                scope,
                &result.tool_hash,
                &json!({"output":result.value,"fuel_used":result.fuel_used,"operations":result.operations}),
            ) {
                result.provenance_warning = Some(format!(
                    "Tool completed; its learning receipt is incomplete (some memory may already be saved): {error}"
                ));
            }
        }
        Ok(result)
    }

    fn require_scope(&self, scope: &str) -> Result<()> {
        if self.redacted {
            bail!("Memory was forgotten; evolution is blocked")
        }
        if let Some(enabled) = self.scope.as_deref() {
            if enabled != scope {
                bail!("This scope is not enabled")
            }
        } else if scope.is_empty() || scope.len() > 256 || scope.chars().any(char::is_control) {
            bail!("This scope is not valid")
        }
        Ok(())
    }

    fn ensure_workshop(&mut self, root: &Path) -> Result<&WorkshopUi> {
        if self.workshop.is_none() {
            let workshop = WorkshopUi::open(root.join("workshop.sqlite"))
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            if let Some(pending) = self.pending_evolution.as_ref() {
                if let Some(candidate_json) = pending.candidate_json.as_deref() {
                    let candidate: CandidateManifest = serde_json::from_str(candidate_json)
                        .map_err(|error| {
                            anyhow::anyhow!("persisted candidate is invalid: {error}")
                        })?;
                    let hash = validate_definition(&candidate.definition)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    workshop
                        .restore_report(&pending.experiment, &hash)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
            }
            self.workshop = Some(workshop);
        }
        self.workshop_root = Some(root.to_path_buf());
        Ok(self.workshop.as_ref().expect("workshop initialized"))
    }

    fn ensure_workshop_stored(&mut self) -> Result<&WorkshopUi> {
        let root = self
            .workshop_root
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Workshop root is not configured"))?;
        self.ensure_workshop(&root)
    }

    pub(crate) fn snapshot(&mut self, scope: &str) -> Value {
        if self.redacted {
            return json!({"state":"unavailable","provider":"none","background_calls":0,"notice":"Cached provider context hidden after forgetting; explicit recovery required."});
        }
        let author = self.author_snapshot(scope);
        if self.service.is_none() || self.scope.as_deref() != Some(scope) {
            return self.snapshot_without_provider(scope, author);
        }
        self.snapshot_with_provider(scope, author)
    }

    fn author_snapshot(&self, scope: &str) -> Value {
        if self.scope.as_deref() != Some(scope) {
            return Value::Null;
        }
        self.author
            .as_ref()
            .map(|author| {
                let snap = author.snapshot();
                let error = snap.error.or(match snap.result {
                    Some(TurnResult::Failed { text, .. }) => Some(text),
                    _ => None,
                });
                json!({"state":format!("{:?}",snap.state),"error":error})
            })
            .unwrap_or(Value::Null)
    }

    fn snapshot_without_provider(&mut self, scope: &str, author: Value) -> Value {
        let _ = self.ensure_workshop_stored();
        let scope_matches_pending = self.pending_scope_matches(scope);
        let (report, needs_approval, state, error) = self
            .workshop
            .as_ref()
            .map(|workshop| empty_provider_workshop_state(workshop, scope, scope_matches_pending))
            .unwrap_or((None, false, "idle".into(), None));
        let comparison = self.comparison_snapshot(scope, report.as_ref());
        json!({"state":"not_enabled","provider":"none","background_calls":0,"author":author,"workshop_state":state,"workshop_error":error,"workshop_report":report,"workshop_comparison":comparison,"needs_approval":needs_approval})
    }

    fn snapshot_with_provider(&self, scope: &str, author: Value) -> Value {
        let service = self.service.as_ref().expect("service checked above");
        let snapshot = service.snapshot();
        let mut workshop_report: Option<EvaluationReport> = None;
        let mut needs_approval = false;
        let mut workshop_state = "idle".to_owned();
        let mut workshop_error: Option<String> = None;
        if let Some(workshop) = self.workshop.as_ref() {
            let ws = workshop.snapshot();
            workshop_state = format!("{:?}", ws.state).to_lowercase();
            workshop_error = ws.error;
            if self.pending_scope_matches(scope) {
                workshop_report = ws.report;
            } else {
                workshop_state = "idle".into();
                workshop_error = None;
            }
            needs_approval = matches!(ws.state, crate::assistant_workshop_ui::UiState::Complete)
                && workshop_report.as_ref().is_some_and(|report| report.passed)
                && workshop_report.as_ref().is_some_and(|report| {
                    !workshop
                        .is_active_hash(&report.tool_hash, ToolScope::new([scope.to_owned()]))
                        .unwrap_or(false)
                });
        }
        let text = match snapshot.result.as_ref() {
            Some(TurnResult::Complete { text, .. }) => text.as_str(),
            _ => snapshot.partial.as_str(),
        };
        let error = snapshot
            .error
            .as_deref()
            .or(match snapshot.result.as_ref() {
                Some(TurnResult::Failed { text, .. }) => Some(text.as_str()),
                _ => None,
            });
        let comparison = self.comparison_snapshot(scope, workshop_report.as_ref());
        json!({"state": match snapshot.state {
            ServiceState::Starting => "starting", ServiceState::Idle => "ready", ServiceState::Running => "working", ServiceState::Cancelling => "cancelling", ServiceState::Completed => "ready", ServiceState::Failed => "unavailable", ServiceState::Stopped => "stopped"
        }, "provider":"codex", "can_reconnect": snapshot.state == ServiceState::Failed && snapshot.request_id.is_none() && !self.redacted && !self.busy(), "background_calls":0, "author":author,"request_id":snapshot.request_id,"user_record_id":snapshot.user_record_id, "partial":text, "error":error, "cost":"unknown; count and deadline bounded", "workshop_state": workshop_state, "workshop_error": workshop_error, "workshop_report": workshop_report,"workshop_comparison":comparison, "needs_approval": needs_approval})
    }

    fn pending_scope_matches(&self, scope: &str) -> bool {
        self.pending_evolution
            .as_ref()
            .is_none_or(|pending| pending.scope == scope)
    }

    fn comparison_snapshot(&self, scope: &str, report: Option<&EvaluationReport>) -> Value {
        if self
            .pending_evolution
            .as_ref()
            .is_none_or(|pending| pending.scope != scope)
        {
            return Value::Null;
        }
        let Some(report) = report else {
            return Value::Null;
        };
        self.workshop
            .as_ref()
            .and_then(|workshop| workshop.comparisons(&report.tool_hash).ok())
            .and_then(|comparisons| serde_json::to_value(comparisons).ok())
            .unwrap_or(Value::Null)
    }

    pub(crate) fn tick(&mut self) -> Result<()> {
        if self.redacted {
            return Ok(());
        }
        self.restore_candidate_workshop()?;
        self.settle_authoring()?;
        self.settle_evaluation()?;
        self.submit_pending_candidate()
    }

    fn restore_candidate_workshop(&mut self) -> Result<()> {
        if self
            .pending_evolution
            .as_ref()
            .is_some_and(|pending| pending.candidate_json.is_some())
            && self.workshop.is_none()
        {
            self.ensure_workshop_stored()?;
        }
        Ok(())
    }

    fn settle_authoring(&mut self) -> Result<()> {
        let snapshot = self.author.as_ref().map(|service| service.snapshot());
        if let (Some(snapshot), Some(pending)) = (snapshot, self.pending_evolution.as_mut()) {
            if pending.scope == self.scope.as_deref().unwrap_or("")
                && pending.request_id == snapshot.request_id.as_deref().unwrap_or("")
                && !pending.submitted
                && !pending.terminal
                && matches!(snapshot.state, ServiceState::Completed)
            {
                if let Some(TurnResult::Complete { text, .. }) = snapshot.result.as_ref() {
                    if let Some(root) = self.workshop_root.as_ref() {
                        if persist_candidate(root, &pending.request_id, text).is_ok() {
                            pending.candidate_json = Some(text.clone());
                        } else {
                            pending.terminal = true;
                        }
                    }
                } else {
                    pending.terminal = true;
                }
            }
            if matches!(snapshot.state, ServiceState::Failed | ServiceState::Stopped) {
                pending.terminal = true;
            }
            if pending.terminal && !pending.submitted {
                if let Some(root) = self.workshop_root.as_ref() {
                    mark_pending_terminal(root, &pending.request_id, "authoring_failed")?;
                }
            }
        }
        Ok(())
    }

    fn settle_evaluation(&mut self) -> Result<()> {
        if let Some(workshop) = self.workshop.as_ref() {
            let workshop_snapshot = workshop.snapshot();
            if matches!(
                workshop_snapshot.state,
                crate::assistant_workshop_ui::UiState::Failed
                    | crate::assistant_workshop_ui::UiState::Closed
            ) {
                if let Some(pending) = self.pending_evolution.as_mut() {
                    pending.terminal = true;
                    pending.evaluation_queued = true;
                    if let Some(root) = self.workshop_root.as_ref() {
                        let _ =
                            mark_pending_terminal(root, &pending.request_id, "evaluation_failed");
                    }
                }
            } else if workshop_snapshot.report.is_some() {
                if let Some(pending) = self.pending_evolution.as_mut() {
                    if pending.submitted {
                        if let (Some(root), Some(report)) = (
                            self.workshop_root.as_ref(),
                            workshop_snapshot.report.as_ref(),
                        ) {
                            crate::assistant_learning::bind_candidate(
                                root,
                                &pending.request_id,
                                &report.tool_hash,
                            )?;
                        }
                        pending.terminal = true;
                        pending.evaluation_queued = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn submit_pending_candidate(&mut self) -> Result<()> {
        let Some(candidate_json) = self
            .pending_evolution
            .as_ref()
            .and_then(|pending| pending.candidate_json.clone())
        else {
            return Ok(());
        };
        if self
            .pending_evolution
            .as_ref()
            .is_some_and(|pending| pending.evaluation_queued || pending.terminal)
        {
            return Ok(());
        }
        let (experiment, scope, request_id) = match self.pending_evolution.as_ref() {
            Some(pending) => (
                pending.experiment.clone(),
                pending.scope.clone(),
                pending.request_id.clone(),
            ),
            None => return Ok(()),
        };
        let assigned = ToolScope::new([scope]);
        // Persist the replayable pure-evaluation intent before enqueueing any
        // work. A failed journal write cannot masquerade as submission.
        let root = self
            .workshop_root
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workshop root missing"))?;
        if let Err(error) = mark_candidate_submitted(root, &request_id) {
            if let Some(workshop) = self.workshop.as_ref() {
                workshop.report_error(format!("Evaluation was not queued: {error}"));
            }
            return Err(error);
        }
        let result = self.workshop.as_ref().map(|workshop| {
            workshop.submit_candidate_json(&experiment, &assigned, &candidate_json)
        });
        match result {
            Some(Ok(())) => {
                if let Some(pending) = self.pending_evolution.as_mut() {
                    pending.submitted = true;
                    pending.evaluation_queued = true;
                }
            }
            Some(Err(_)) | None => {
                if let Some(pending) = self.pending_evolution.as_mut() {
                    pending.terminal = true;
                    if let Some(root) = self.workshop_root.as_ref() {
                        let _ =
                            mark_pending_terminal(root, &pending.request_id, "evaluation_failed");
                    }
                }
            }
        }
        Ok(())
    }
}

fn empty_provider_workshop_state(
    workshop: &WorkshopUi,
    scope: &str,
    scope_matches_pending: bool,
) -> (Option<EvaluationReport>, bool, String, Option<String>) {
    let snapshot = workshop.snapshot();
    let report = scope_matches_pending.then_some(snapshot.report).flatten();
    let needs_approval = matches!(
        snapshot.state,
        crate::assistant_workshop_ui::UiState::Complete
    ) && report.as_ref().is_some_and(|item| item.passed)
        && report.as_ref().is_some_and(|item| {
            !workshop
                .is_active_hash(&item.tool_hash, ToolScope::new([scope.to_owned()]))
                .unwrap_or(false)
        });
    let state = format!("{:?}", snapshot.state).to_lowercase();
    if scope_matches_pending {
        (report, needs_approval, state, snapshot.error)
    } else {
        (None, false, "idle".into(), None)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EvolutionSpec {
    need: String,
    cases: Vec<crate::assistant_evolution::EvaluationCase>,
    #[serde(default)]
    correction_id: Option<String>,
}

fn parse_evolution_spec(source: &str) -> Result<EvolutionSpec> {
    let spec: EvolutionSpec = serde_json::from_str(source)
        .map_err(|error| anyhow::anyhow!("invalid evolution specification: {error}"))?;
    if spec.need.trim().is_empty() || spec.need.len() > 4096 || !(2..=8).contains(&spec.cases.len())
    {
        bail!("need and 2–8 contrasting cases are required");
    }
    Ok(spec)
}

fn validate_evolution_scope(spec: &EvolutionSpec, scope: &str) -> Result<()> {
    let assigned = ToolScope::new([scope.to_owned()]);
    if spec
        .cases
        .iter()
        .flat_map(|case| case.inputs.iter())
        .any(|input| input.scope != assigned)
    {
        bail!("every case input must exactly match the assigned scope");
    }
    Ok(())
}

fn experiment_id(
    scope: &str,
    cases: &[crate::assistant_evolution::EvaluationCase],
) -> Result<String> {
    let encoded = serde_json::to_vec(cases)?;
    let mut digest = sha2::Sha256::new();
    digest.update(scope.as_bytes());
    digest.update(&encoded);
    Ok(format!("evolve-{:x}", digest.finalize()))
}

fn persist_evolution_cases(
    session: &Session,
    root: &Path,
    scope: &str,
    request_id: &str,
    experiment: &str,
    spec: &EvolutionSpec,
) -> Result<()> {
    let protected_cases = json!({"experiment": experiment, "cases": spec.cases});
    persist_pending(root, request_id, scope, experiment, None)?;
    if let Err(error) = session
        .workshop
        .as_ref()
        .expect("workshop initialized")
        .protect_cases_json(&protected_cases.to_string())
    {
        let _ = mark_pending_terminal(root, request_id, "cases_failed");
        return Err(anyhow::anyhow!(error.to_string()));
    }
    if let Some(correction_id) = spec.correction_id.as_deref() {
        if let Err(error) = crate::assistant_learning::register_hypothesis(
            root,
            scope,
            request_id,
            correction_id,
            &spec.need,
        ) {
            mark_pending_terminal(root, request_id, "hypothesis_failed")?;
            return Err(error.into());
        }
    }
    Ok(())
}

fn evolution_prompt(scope: &str, need: &str) -> String {
    format!(
        "Generate exactly one CandidateManifest JSON object with definition.name string, version integer, input_scope {{values: [string]}}, expression, and authoring_evidence string. The expression AST grammar is tagged JSON: {{op: \"input\"}}, {{op: \"current\"}}, {{op: \"field\",path:string}}, {{op: \"current_field\",path:string}}, {{op: \"literal\",value:any}}, {{op: \"map\",input:Expr,expr:Expr}}, {{op: \"filter\",input:Expr,predicate:Expr}}, {{op: \"compare\",comparison: \"eq\"|\"ne\"|\"lt\"|\"lte\"|\"gt\"|\"gte\",left:Expr,right:Expr}}, {{op: \"and\"|\"or\",items:[Expr]}}, or {{op: \"not\",input:Expr}}. For assigned scope [\"{scope}\"], solve this need: {need}. Do not return evaluator cases or expected values, approvals, grants, commands, tools, or prose."
    )
}

fn validate_enable(session: &Session, root: &Path, calls: u64) -> Result<()> {
    if session.service.as_ref().is_some_and(|service| {
        let state = service.snapshot();
        state.state != ServiceState::Failed
            || state.request_id.is_some()
            || session.redacted
            || session.busy()
    }) {
        bail!(
            "Pika already has a foreground provider; close all assistant views before changing it"
        );
    }
    if crate::assistant_recovery_service::has_unfinished(root)? {
        bail!(
            "An earlier recovery did not finish. Use /fresh-context acknowledge before enabling another provider; no old request will be replayed."
        );
    }
    if !(1..=100).contains(&calls) && calls != crate::assistant_policy::NO_CALL_LIMIT {
        bail!("Choose --max-calls 1–100 or --no-call-limit; neither sets a monetary ceiling");
    }
    Ok(())
}

fn transport_config(root: &Path, executable: &Path) -> Result<TransportConfig> {
    // Provider auth is separately provisioned here, never copied from the
    // user's existing provider profile. User input cannot redirect it.
    let home = root.join("provider-home");
    let scratch = root.join("scratch");
    crate::assistant_storage::directory(&home)?;
    crate::assistant_storage::directory(&scratch)?;
    let config = TransportConfig {
        executable: executable.to_path_buf(),
        codex_home: home,
        scratch,
    };
    config.validate()?;
    Ok(config)
}

fn project_scope(name: &str) -> Scope {
    Scope {
        project: Some(name.to_owned()),
        ..Scope::default()
    }
}

fn spawn_main_service(
    root: PathBuf,
    config: TransportConfig,
    scope: Scope,
    calls: u64,
    background_calls: u64,
) -> AssistantService {
    AssistantService::spawn(move || {
        let make = || -> Result<_> {
            let memory = Store::open(root.join("memory.sqlite"))?;
            let policy = AssistantPolicy::open(root.join("policy.sqlite"))?;
            let journal = root.join("runtime.sqlite");
            let thread_id = persisted_thread(&journal)?;
            let factory =
                crate::assistant_investigation_provider::ScopedWorkerFactory::from_permission_root(
                    crate::assistant_investigation_provider::CodexWorkerFactory::new(
                        config.executable.clone(),
                        &root,
                    )
                    .map_err(anyhow::Error::msg)?,
                    &root,
                )
                .map_err(anyhow::Error::msg)?;
            let provider = MainAssistant::new(
                CodexTransport::spawn(config)?,
                MainProfile {
                    profile_id: memory.profile_id().into(),
                    thread_id,
                },
            );
            let mut runtime = AssistantRuntime::open(provider, memory, policy, journal, scope)?;
            runtime.configure_explicit(RuntimeConfig {
                max_calls: calls,
                ..RuntimeConfig::default()
            })?;
            runtime.start_or_resume(now())?;
            let coordinated = crate::assistant_investigation_service::CoordinatedRuntime::new(
                runtime, factory, &root,
            )
            .map_err(anyhow::Error::msg)?;
            if background_calls > 0 {
                Ok(coordinated
                    .with_background_budget(background_calls)
                    .map_err(anyhow::Error::msg)?)
            } else {
                Ok(coordinated)
            }
        };
        make().map_err(|error| format!("{error:#}"))
    })
}

fn persist_pending(
    root: &Path,
    request_id: &str,
    scope: &str,
    experiment: &str,
    candidate_json: Option<&str>,
) -> Result<()> {
    let path = root.join("workshop.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = rusqlite::Connection::open(path)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS assistant_evolution_pending (request_id TEXT PRIMARY KEY, scope TEXT NOT NULL, experiment TEXT NOT NULL, candidate_json BLOB, state TEXT NOT NULL, updated_at INTEGER NOT NULL)")?;
    db.execute("INSERT INTO assistant_evolution_pending(request_id,scope,experiment,candidate_json,state,updated_at) VALUES(?,?,?,?, 'pending', strftime('%s','now'))", rusqlite::params![request_id, scope, experiment, candidate_json])?;
    Ok(())
}

fn persist_candidate(root: &Path, request_id: &str, candidate_json: &str) -> Result<()> {
    let path = root.join("workshop.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = rusqlite::Connection::open(path)?;
    db.execute("UPDATE assistant_evolution_pending SET candidate_json=?,state='candidate',updated_at=strftime('%s','now') WHERE request_id=?", rusqlite::params![candidate_json, request_id])?;
    Ok(())
}

fn mark_candidate_submitted(root: &Path, request_id: &str) -> Result<()> {
    let path = root.join("workshop.sqlite");
    let db = rusqlite::Connection::open(path)?;
    db.execute("UPDATE assistant_evolution_pending SET state='submitted',updated_at=strftime('%s','now') WHERE request_id=?", rusqlite::params![request_id])?;
    Ok(())
}

fn mark_pending_terminal(root: &Path, request_id: &str, state: &str) -> Result<()> {
    let path = root.join("workshop.sqlite");
    let db = rusqlite::Connection::open(path)?;
    db.execute("UPDATE assistant_evolution_pending SET state=?,updated_at=strftime('%s','now') WHERE request_id=?", rusqlite::params![state, request_id])?;
    Ok(())
}

type StoredPending = (String, String, String, Option<String>, String);
fn load_pending(root: &Path) -> Result<Option<StoredPending>> {
    let Some(db) = open_pending_database(root)? else {
        return Ok(None);
    };
    let mut query = db.prepare("SELECT request_id,scope,experiment,candidate_json,state FROM assistant_evolution_pending WHERE state IN ('pending','candidate','submitted') ORDER BY updated_at DESC,rowid DESC LIMIT 1")?;
    let Some((request_id, scope, experiment, mut candidate_json, state)): Option<(
        String,
        String,
        String,
        Option<String>,
        String,
    )> = query
        .query_row([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .optional()?
    else {
        return Ok(None);
    };
    if candidate_json.is_none()
        && !recover_candidate_from_runtime(root, &db, &request_id, &mut candidate_json)?
    {
        return Ok(None);
    }
    Ok(Some((request_id, scope, experiment, candidate_json, state)))
}

fn open_pending_database(root: &Path) -> Result<Option<rusqlite::Connection>> {
    let path = root.join("workshop.sqlite");
    if !path.exists() {
        return Ok(None);
    }
    crate::assistant_storage::database(&path)?;
    let db = rusqlite::Connection::open(path)?;
    let has_table: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='assistant_evolution_pending')",
        [],
        |row| row.get(0),
    )?;
    Ok(has_table.then_some(db))
}

fn recover_candidate_from_runtime(
    root: &Path,
    pending_db: &rusqlite::Connection,
    request_id: &str,
    candidate_json: &mut Option<String>,
) -> Result<bool> {
    let Some((state, reply)) = read_author_runtime(root, request_id)? else {
        abandon_unsent(pending_db, request_id)?;
        return Ok(false);
    };
    if state == "completed" {
        *candidate_json = reply;
    } else if is_terminal_author_state(&state) {
        mark_pending_terminal(root, request_id, "authoring_failed")?;
        return Ok(false);
    }
    Ok(true)
}

fn read_author_runtime(root: &Path, request_id: &str) -> Result<Option<(String, Option<String>)>> {
    let runtime = root.join("author-runtime.sqlite");
    if !runtime.exists() {
        return Ok(None);
    }
    crate::assistant_storage::database(&runtime)?;
    let runtime_db = rusqlite::Connection::open(runtime)?;
    if !runtime_table_exists(&runtime_db)? {
        return Ok(None);
    }
    Ok(runtime_db
        .query_row(
            "SELECT state,reply FROM assistant_runtime_turns WHERE request_id=?",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn runtime_table_exists(db: &rusqlite::Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='assistant_runtime_turns')",
        [], |row| row.get(0),
    )?)
}

fn is_terminal_author_state(state: &str) -> bool {
    matches!(state, "failed" | "cancelled" | "denied" | "abandoned")
}

fn abandon_unsent(db: &rusqlite::Connection, request_id: &str) -> Result<()> {
    // Do not replay an intent with no durable provider dispatch receipt.
    db.execute(
        "UPDATE assistant_evolution_pending SET state='abandoned-unsent',updated_at=strftime('%s','now') WHERE request_id=?",
        [request_id],
    )?;
    Ok(())
}

fn persisted_thread(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    crate::assistant_storage::database(path)?;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    use rusqlite::OptionalExtension;
    Ok(db
        .query_row(
            "SELECT thread_id FROM assistant_runtime_profile WHERE id=1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
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
    use crate::assistant_evolution::{EvaluationCase, Expr, ToolDefinition};
    use crate::assistant_service::LiveTurnRuntime;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    #[test]
    fn failed_initialization_can_reconnect_but_forgotten_context_cannot() {
        let root = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut session = Session::new();
        session.service = Some(AssistantService::spawn(
            || -> std::result::Result<FakeRuntime, String> { Err("startup failed".into()) },
        ));
        for _ in 0..100 {
            if session.service.as_ref().unwrap().snapshot().state == ServiceState::Failed {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        validate_enable(&session, root.path(), 12).unwrap();
        session.redacted = true;
        assert!(validate_enable(&session, root.path(), 12).is_err());
    }

    #[test]
    fn enable_accepts_uncapped_lifetime_only_as_an_explicit_allowance() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        let session = Session::new();
        for calls in [1, 12, 100, crate::assistant_policy::NO_CALL_LIMIT] {
            validate_enable(&session, &root, calls).unwrap();
        }
        for calls in [0, 101, u64::MAX] {
            assert!(validate_enable(&session, &root, calls).is_err());
        }
    }

    struct FakeRuntime {
        responses: Vec<String>,
        response: String,
        calls: Arc<AtomicUsize>,
        done: bool,
    }
    impl LiveTurnRuntime for FakeRuntime {
        fn begin_turn(&mut self, _: &str, _: &str, _: i64) -> std::result::Result<(), String> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(response) = self.responses.get(index) {
                self.response = response.clone();
            }
            self.done = false;
            Ok(())
        }
        fn poll_turn(&mut self, _: i64) -> std::result::Result<Option<TurnResult>, String> {
            if self.done {
                return Ok(None);
            }
            self.done = true;
            Ok(Some(TurnResult::Complete {
                turn_id: "turn".into(),
                text: self.response.clone(),
                usage: None,
            }))
        }
        fn cancel(&mut self, _: i64) -> std::result::Result<(), String> {
            self.done = true;
            Ok(())
        }
    }

    fn candidate_json(scope: &str, version: u32) -> String {
        serde_json::to_string(&crate::assistant_evolution::CandidateManifest {
            definition: ToolDefinition {
                name: format!("attention-title-{scope}"),
                version: version.into(),
                input_scope: ToolScope::new([scope.to_owned()]),
                expression: Expr::Map {
                    input: Box::new(Expr::Filter {
                        input: Box::new(Expr::Input),
                        predicate: Box::new(Expr::Compare {
                            comparison: crate::assistant_evolution::CompareOp::Eq,
                            left: Box::new(Expr::CurrentField {
                                path: "status".into(),
                            }),
                            right: Box::new(Expr::Literal {
                                value: json!("needs_you"),
                            }),
                        }),
                    }),
                    expr: Box::new(Expr::CurrentField {
                        path: "title".into(),
                    }),
                },
            },
            authoring_evidence: "fake candidate".into(),
        })
        .unwrap()
    }

    #[test]
    fn project_scoped_authoring_catalog_and_retirement_use_no_main_turn() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        let calls = Arc::new(AtomicUsize::new(0));
        let service = AssistantService::spawn({
            let calls = calls.clone();
            move || {
                Ok(FakeRuntime {
                    responses: vec![],
                    response: candidate_json("alpha", 1),
                    calls,
                    done: false,
                })
            }
        });
        let mut session = Session::with_service_for_test(service, "alpha");
        let spec = json!({"need":"Select attention titles", "cases":[{"inputs":[{"value":{"status":"needs_you","title":"Yes"},"scope":ToolScope::new(["alpha"])}],"expected":["Yes"]},{"inputs":[],"expected":[]}]});
        assert!(
            session
                .evolve_spec(&root, "beta", "wrong-scope", &spec.to_string())
                .is_err()
        );
        session
            .evolve_spec(&root, "alpha", "alpha-author", &spec.to_string())
            .unwrap();
        for _ in 0..100 {
            session.tick().unwrap();
            if session.snapshot("alpha")["workshop_report"].is_object() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let snapshot = session.snapshot("alpha");
        assert_eq!(snapshot["workshop_report"]["passed"], true);
        let hash = snapshot["workshop_report"]["tool_hash"]
            .as_str()
            .unwrap()
            .to_owned();
        session.approve_evolution("alpha", &hash).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(session);
        let mut session = Session::with_workshop_for_test("alpha", &root);
        session.set_workshop_root(&root).unwrap();
        let catalog = session.tool_catalog("alpha", Some(&hash)).unwrap();
        assert_eq!(
            catalog["tools"][0]["definition"]["name"],
            "attention-title-alpha"
        );
        assert!(session.tool_catalog("beta", None).is_err());
        assert_eq!(
            session
                .invoke_evolution(
                    "alpha",
                    "attention-title-alpha",
                    r#"[{"status":"needs_you","title":"Withheld"}]"#
                )
                .unwrap()
                .value,
            json!(["Withheld"])
        );
        session
            .assess_evolution(
                "alpha",
                &hash,
                "regression",
                "Misleading real-world grouping",
                None,
            )
            .unwrap();
        assert!(
            session
                .invoke_evolution("alpha", "attention-title-alpha", "[]")
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn correction_drives_hypothesis_candidate_and_later_use_without_main_turn() {
        use crate::assistant_memory::{NewRecord, Origin, RecordKind};
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        let scope = Scope {
            project: Some("personal".into()),
            ..Default::default()
        };
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        let original = memory
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Worker,
                scope: scope.clone(),
                body: "All rows require attention".into(),
                provenance: "synthetic fixture".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let correction = memory
            .append(NewRecord {
                kind: RecordKind::Correction,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "Only needs_you rows belong in my attention list".into(),
                provenance: "explicit correction".into(),
                timestamp: 2,
                supersedes: Some(original.id.clone()),
                dependencies: vec![original.id],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = AssistantService::spawn({
            let calls = calls.clone();
            move || {
                Ok(FakeRuntime {
                    responses: vec![],
                    response: candidate_json("personal", 1),
                    calls,
                    done: false,
                })
            }
        });
        let mut session = Session::with_service_for_test(service, "personal");
        let spec = json!({"correction_id":correction.id,"need":"Select only needs_you row titles", "cases":[
            {"inputs":[{"value":{"status":"needs_you","title":"Yes"},"scope":ToolScope::new(["personal"])}],"expected":["Yes"]},
            {"inputs":[{"value":{"status":"done","title":"No"},"scope":ToolScope::new(["personal"])}],"expected":[]}
        ]});
        persist_pending(&root, "fixture-only", "personal", "fixture", None).unwrap();
        let db = rusqlite::Connection::open(root.join("workshop.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER reject_intent BEFORE INSERT ON assistant_evolution_pending WHEN NEW.request_id='reject-intent' BEGIN SELECT RAISE(FAIL,'intent write failed'); END;").unwrap();
        assert!(
            session
                .evolve_spec(&root, "personal", "reject-intent", &spec.to_string())
                .is_err()
        );
        assert!(
            !memory
                .recent(&scope, 32)
                .unwrap()
                .iter()
                .any(|record| record.kind == RecordKind::Proposal)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        db.execute_batch("CREATE TRIGGER reject_cases BEFORE INSERT ON protected_suites BEGIN SELECT RAISE(FAIL,'case protection failed'); END;").unwrap();
        assert!(
            session
                .evolve_spec(&root, "personal", "reject-cases", &spec.to_string())
                .is_err()
        );
        assert!(
            !memory
                .recent(&scope, 32)
                .unwrap()
                .iter()
                .any(|record| record.kind == RecordKind::Proposal)
        );
        db.execute_batch("DROP TRIGGER reject_cases").unwrap();
        session
            .evolve_spec(
                &root,
                "personal",
                "correction-experiment",
                &spec.to_string(),
            )
            .unwrap();
        for _ in 0..200 {
            session.tick().unwrap();
            if session.snapshot("personal")["workshop_report"].is_object() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        session.tick().unwrap();
        let view = session.snapshot("personal");
        assert_eq!(view["workshop_report"]["passed"], true);
        let hash = view["workshop_report"]["tool_hash"].as_str().unwrap();
        session.approve_evolution("personal", hash).unwrap();
        drop(session);
        let mut reopened = Session::with_workshop_for_test("personal", &root);
        reopened.set_workshop_root(&root).unwrap();
        let output=reopened.invoke_evolution("personal","attention-title-personal",r#"[{"status":"needs_you","title":"Fresh work"},{"status":"done","title":"Ignore"}]"#).unwrap();
        assert_eq!(output.value, json!(["Fresh work"]));
        assert_eq!(output.tool_hash, hash);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let records = memory.recent(&scope, 32).unwrap();
        let hypothesis = records
            .iter()
            .find(|record| {
                record.kind == RecordKind::Proposal && record.dependencies.contains(&correction.id)
            })
            .unwrap();
        let use_record = records
            .iter()
            .find(|record| {
                record.kind == RecordKind::Finding && record.dependencies.contains(&hypothesis.id)
            })
            .unwrap();
        assert!(use_record.body.contains(hash));
        assert!(use_record.body.contains("unknown"));
        let hypothesis_id = hypothesis.id.clone();
        let use_id = use_record.id.clone();
        let learning_db = rusqlite::Connection::open(root.join("learning.sqlite")).unwrap();
        learning_db.execute_batch("CREATE TRIGGER fail_use BEFORE INSERT ON learning_uses BEGIN SELECT RAISE(FAIL,'receipt failed'); END;").unwrap();
        let warned = reopened
            .invoke_evolution("personal", "attention-title-personal", "[]")
            .unwrap();
        assert_eq!(warned.value, json!([]));
        assert!(
            warned
                .provenance_warning
                .as_deref()
                .unwrap()
                .contains("Tool completed")
        );
        memory.forget(&correction.id).unwrap();
        crate::assistant_retention::cleanup(&root, memory.forget_epoch().unwrap()).unwrap();
        assert!(memory.get(&hypothesis_id).unwrap().is_none());
        assert!(memory.get(&use_id).unwrap().is_none());
        assert!(
            reopened
                .invoke_evolution("personal", "attention-title-personal", "[]")
                .is_err()
        );
    }

    #[test]
    fn default_has_no_provider_and_cannot_send_or_cross_scope() {
        let mut session = Session::new();
        assert_eq!(session.snapshot("personal")["provider"], "none");
        assert!(session.begin("personal", "r", "hello").is_err());
        session.cancel().unwrap();
    }

    #[test]
    fn evolution_requires_scope_and_explicit_exact_approval() {
        let temporary = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let response = candidate_json("personal", 1);
        let second_response = candidate_json("personal", 2);
        let second_manifest: CandidateManifest = serde_json::from_str(&second_response).unwrap();
        let second_hash = validate_definition(&second_manifest.definition).unwrap();
        let service = AssistantService::spawn({
            let calls = calls.clone();
            move || {
                Ok(FakeRuntime {
                    responses: vec![response.clone(), second_response],
                    response,
                    calls,
                    done: false,
                })
            }
        });
        let mut session = Session::with_service_for_test(service, "personal");
        let main_calls = Arc::new(AtomicUsize::new(0));
        session.service = Some(AssistantService::spawn({
            let calls = main_calls.clone();
            move || {
                Ok(FakeRuntime {
                    responses: vec![],
                    response: "main untouched".into(),
                    calls,
                    done: false,
                })
            }
        }));
        for _ in 0..100 {
            if session.service.as_ref().unwrap().snapshot().state == ServiceState::Idle {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let root = temporary.path().join("private");
        assert!(session.evolve(&root, "other", "bad").is_err());
        session.evolve(&root, "personal", "evolve-1").unwrap();
        for _ in 0..100 {
            session.tick().unwrap();
            let view = session.snapshot("personal");
            if view["workshop_report"].is_object() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let view = session.snapshot("personal");
        let report: EvaluationReport =
            serde_json::from_value(view["workshop_report"].clone()).unwrap();
        assert!(report.passed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(main_calls.load(Ordering::SeqCst), 0);
        assert_eq!(view["workshop_comparison"][0]["verdict"], "first_candidate");
        assert!(
            session
                .invoke_evolution("personal", "attention-title-personal", "[]")
                .is_err()
        );
        session
            .approve_evolution("personal", &report.tool_hash)
            .unwrap();
        assert!(
            session
                .invoke_evolution("personal", "attention-title-personal", "[]")
                .is_ok()
        );
        let altered = json!({"need":"altered", "cases":[
            {"inputs":[{"value":{"status":"needs_you","title":"First"},"scope":ToolScope::new(["personal"])}],"expected":["First"]},
            {"inputs":[{"value":{"status":"done","title":"Skip"},"scope":ToolScope::new(["personal"])}],"expected":[]}
        ]});
        assert!(
            session
                .evolve_spec(&root, "personal", "evolve-1", &altered.to_string())
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let generic_spec = json!({
            "need": "select attention titles",
            "cases": [
                {"inputs": [{"value": {"status":"needs_you","title":"Generic"}, "scope": ToolScope::new(["personal"]) }], "expected": ["Generic"]},
                {"inputs": [{"value": {"status":"done","title":"Nope"}, "scope": ToolScope::new(["personal"]) }], "expected": []}
            ]
        });
        session
            .evolve_spec(&root, "personal", "evolve-2", &generic_spec.to_string())
            .unwrap();
        for _ in 0..100 {
            session.tick().unwrap();
            let view = session.snapshot("personal");
            if view["workshop_report"]["tool_hash"].as_str() == Some(second_hash.as_str()) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            session.snapshot("personal")["workshop_report"]["tool_hash"],
            second_hash
        );
        assert_eq!(
            session.snapshot("personal")["workshop_report"]["passed"],
            true
        );
        let excluded = session.snapshot("excluded");
        assert!(excluded["workshop_report"].is_null());
        assert!(excluded["workshop_error"].is_null());
        drop(session);
        let mut reopened = Session::with_workshop_for_test("personal", &root);
        reopened.set_workshop_root(&root).unwrap();
        assert!(
            reopened
                .invoke_evolution("personal", "attention-title-personal", "[]")
                .is_ok()
        );
        let before_revoke = reopened.snapshot("personal");
        let reopened_report: EvaluationReport =
            serde_json::from_value(before_revoke["workshop_report"].clone()).unwrap();
        let grant_id = reopened
            .approve_evolution("personal", &reopened_report.tool_hash)
            .unwrap();
        assert_eq!(reopened.snapshot("personal")["needs_approval"], false);
        reopened.revoke_evolution("personal", &grant_id).unwrap();
        assert_eq!(reopened.snapshot("personal")["needs_approval"], true);
    }

    #[test]
    fn cancelled_pending_evolution_is_not_resent() {
        let temporary = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let response = candidate_json("personal", 1);
        let service = AssistantService::spawn({
            let calls = calls.clone();
            move || {
                Ok(FakeRuntime {
                    responses: vec![response.clone()],
                    response,
                    calls,
                    done: false,
                })
            }
        });
        let mut session = Session::with_service_for_test(service, "personal");
        session
            .evolve(
                &temporary.path().join("private"),
                "personal",
                "evolve-cancel",
            )
            .unwrap();
        session.cancel().unwrap();
        for _ in 0..20 {
            session.tick().unwrap();
            let _ = session.snapshot("personal");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(calls.load(Ordering::SeqCst) <= 1);
        assert!(
            session
                .pending_evolution
                .as_ref()
                .is_some_and(|pending| pending.terminal)
        );
    }

    #[test]
    fn offline_snapshot_handles_registry_database_without_pending_table() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        crate::assistant_workshop::Workshop::open(&root.join("workshop.sqlite")).unwrap();
        let mut session = Session::with_workshop_for_test("personal", &root);
        session.set_workshop_root(&root).unwrap();
        let view = session.snapshot("personal");
        assert_eq!(view["provider"], "none");
        assert_eq!(view["state"], "not_enabled");
        assert!(view["workshop_error"].is_null());
    }

    #[test]
    fn failed_authoring_is_visible_and_terminal_across_restart() {
        struct Fails;
        impl LiveTurnRuntime for Fails {
            fn begin_turn(&mut self, _: &str, _: &str, _: i64) -> std::result::Result<(), String> {
                Ok(())
            }
            fn poll_turn(&mut self, _: i64) -> std::result::Result<Option<TurnResult>, String> {
                Ok(Some(TurnResult::Failed {
                    turn_id: "failed-turn".into(),
                    text: "provider declined".into(),
                }))
            }
            fn cancel(&mut self, _: i64) -> std::result::Result<(), String> {
                Ok(())
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let mut session =
            Session::with_service_for_test(AssistantService::spawn(|| Ok(Fails)), "personal");
        session.evolve(&root, "personal", "failed-author").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            session.tick().unwrap();
            if session
                .pending_evolution
                .as_ref()
                .is_some_and(|p| p.terminal)
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            session.snapshot("personal")["author"]["error"],
            "provider declined"
        );
        drop(session);
        assert!(load_pending(&root).unwrap().is_none());
        let db = rusqlite::Connection::open(root.join("workshop.sqlite")).unwrap();
        let state: String = db
            .query_row(
                "SELECT state FROM assistant_evolution_pending WHERE request_id='failed-author'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "authoring_failed");
    }

    #[test]
    fn restart_retires_unsent_intent_but_preserves_unknown_dispatch() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        crate::assistant_storage::directory(&root).unwrap();
        persist_pending(&root, "never-sent", "personal", "experiment", None).unwrap();
        assert!(load_pending(&root).unwrap().is_none());
        let db = rusqlite::Connection::open(root.join("workshop.sqlite")).unwrap();
        let state: String = db
            .query_row(
                "SELECT state FROM assistant_evolution_pending WHERE request_id='never-sent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "abandoned-unsent");
        persist_pending(&root, "unknown-send", "personal", "experiment", None).unwrap();
        let runtime_path = root.join("author-runtime.sqlite");
        crate::assistant_storage::database(&runtime_path).unwrap();
        let runtime = rusqlite::Connection::open(runtime_path).unwrap();
        runtime.execute_batch("CREATE TABLE assistant_runtime_turns(request_id TEXT PRIMARY KEY,state TEXT,reply TEXT); INSERT INTO assistant_runtime_turns VALUES('unknown-send','dispatch_intent',NULL)").unwrap();
        let pending = load_pending(&root).unwrap().unwrap();
        assert_eq!(pending.0, "unknown-send");
        assert!(pending.3.is_none());
        assert_eq!(pending.4, "pending");
        runtime
            .execute(
                "UPDATE assistant_runtime_turns SET state='failed' WHERE request_id='unknown-send'",
                [],
            )
            .unwrap();
        assert!(load_pending(&root).unwrap().is_none());
    }

    #[test]
    fn submitted_candidate_recovers_without_provider_turn() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("private");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let experiment = "submitted-recovery";
        let scope = ToolScope::new(["personal"]);
        let cases = vec![
            EvaluationCase {
                inputs: vec![ScopedInput {
                    value: json!({"status":"needs_you","title":"Keep"}),
                    scope: scope.clone(),
                }],
                expected: json!(["Keep"]),
            },
            EvaluationCase {
                inputs: vec![ScopedInput {
                    value: json!({"status":"done","title":"Skip"}),
                    scope: scope.clone(),
                }],
                expected: json!([]),
            },
        ];
        let workshop =
            crate::assistant_workshop::Workshop::open(&root.join("workshop.sqlite")).unwrap();
        workshop.protect_cases(experiment, &cases).unwrap();
        let candidate = candidate_json("personal", 7);
        let request_id = "recover-submitted";
        persist_pending(&root, request_id, "personal", experiment, Some(&candidate)).unwrap();
        persist_candidate(&root, request_id, &candidate).unwrap();
        mark_candidate_submitted(&root, request_id).unwrap();

        let mut session = Session::with_workshop_for_test("personal", &root);
        session.set_workshop_root(&root).unwrap();
        let db = rusqlite::Connection::open(root.join("workshop.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_submission BEFORE UPDATE ON assistant_evolution_pending WHEN NEW.state='submitted' BEGIN SELECT RAISE(FAIL,'submission write failed'); END;").unwrap();
        assert!(session.tick().is_err());
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT count(*) FROM tool_evaluations", [], |row| row
                .get(0))
                .unwrap(),
            0
        );
        assert!(
            session.snapshot("personal")["workshop_error"]
                .as_str()
                .unwrap()
                .contains("not queued")
        );
        drop(session);
        db.execute_batch("DROP TRIGGER fail_submission").unwrap();
        let mut session = Session::with_workshop_for_test("personal", &root);
        session.set_workshop_root(&root).unwrap();
        for _ in 0..100 {
            session.tick().unwrap();
            if session.snapshot("personal")["workshop_report"].is_object() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let view = session.snapshot("personal");
        assert!(view["workshop_report"].is_object());
        assert_eq!(view["provider"], "none");
    }
}
