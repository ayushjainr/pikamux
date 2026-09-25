//! Small host-facing boundary for the trusted workshop.
//!
//! Provider text is accepted only as a `CandidateManifest` JSON value.  It
//! cannot provide evaluator cases, approvals, grants, or executable actions.
//! The host owns protected cases and every mutating operation below is explicit.

use crate::assistant_evolution::{
    CandidateManifest, EvaluationCase, EvaluationReport, EvolutionError, Scope, ScopedInput,
    ToolResult,
};
use crate::assistant_workshop::Workshop;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_UI_JSON_BYTES: usize = 32 * 1024;
const MAX_CASES: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum WorkshopUiError {
    #[error("request is too large")]
    TooLarge,
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("workshop operation failed: {0}")]
    Workshop(#[from] EvolutionError),
    #[error("evaluation worker is busy")]
    Busy,
    #[error("evaluation worker is closed")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiState {
    Idle,
    Running,
    Complete,
    Failed,
    Closed,
}

#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub state: UiState,
    pub report: Option<EvaluationReport>,
    pub error: Option<String>,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            state: UiState::Idle,
            report: None,
            error: None,
        }
    }
}

struct CandidateJob {
    experiment: String,
    candidate: CandidateManifest,
    cancel: Arc<AtomicBool>,
}

pub struct WorkshopUi {
    workshop: Arc<Workshop>,
    jobs: mpsc::SyncSender<CandidateJob>,
    snapshot: Arc<RwLock<UiSnapshot>>,
    stop: Arc<AtomicBool>,
    current_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    join: Option<JoinHandle<()>>,
}

impl WorkshopUi {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, WorkshopUiError> {
        let workshop = Arc::new(Workshop::open(path.as_ref())?);
        let (jobs, queue) = mpsc::sync_channel::<CandidateJob>(1);
        let snapshot = Arc::new(RwLock::new(UiSnapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let current_cancel = Arc::new(Mutex::new(None));
        let worker_snapshot = snapshot.clone();
        let worker_stop = stop.clone();
        let worker_workshop = workshop.clone();
        let worker_current_cancel = current_cancel.clone();
        let join = thread::Builder::new()
            .name("pika-workshop-evaluator".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    let Ok(job) = queue.recv_timeout(Duration::from_millis(25)) else {
                        continue;
                    };
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    set_snapshot(
                        &worker_snapshot,
                        UiSnapshot {
                            state: UiState::Running,
                            report: None,
                            error: None,
                        },
                    );
                    let result = worker_workshop.submit_candidate(
                        &job.experiment,
                        &job.candidate,
                        Some(&job.cancel),
                    );
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    set_snapshot(
                        &worker_snapshot,
                        match result {
                            Ok(report) => UiSnapshot {
                                state: UiState::Complete,
                                report: Some(report),
                                error: None,
                            },
                            Err(error) => UiSnapshot {
                                state: UiState::Failed,
                                report: None,
                                error: Some(error.to_string()),
                            },
                        },
                    );
                    *worker_current_cancel.lock().expect("workshop UI cancel") = None;
                }
                set_snapshot(
                    &worker_snapshot,
                    UiSnapshot {
                        state: UiState::Closed,
                        report: None,
                        error: None,
                    },
                );
            })
            .map_err(|error| WorkshopUiError::Invalid(format!("worker: {error}")))?;
        Ok(Self {
            workshop,
            jobs,
            snapshot,
            stop,
            current_cancel,
            join: Some(join),
        })
    }

    pub fn snapshot(&self) -> UiSnapshot {
        self.snapshot.read().expect("workshop UI snapshot").clone()
    }

    pub fn report_error(&self, error: String) {
        set_snapshot(
            &self.snapshot,
            UiSnapshot {
                state: UiState::Failed,
                report: None,
                error: Some(error),
            },
        );
    }

    pub fn comparisons(
        &self,
        hash: &str,
    ) -> Result<Vec<crate::assistant_evolution::ComparisonReceipt>, WorkshopUiError> {
        Ok(self.workshop.comparisons(hash)?)
    }

    pub fn restore_report(
        &self,
        experiment: &str,
        candidate_hash: &str,
    ) -> Result<(), WorkshopUiError> {
        if let Some(report) = self
            .workshop
            .latest_evaluation(experiment, candidate_hash)?
        {
            set_snapshot(
                &self.snapshot,
                UiSnapshot {
                    state: UiState::Complete,
                    report: Some(report),
                    error: None,
                },
            );
        }
        Ok(())
    }

    /// Protected cases are host-authored and require contrasting coverage.
    pub fn protect_cases_json(&self, request_json: &str) -> Result<(), WorkshopUiError> {
        if request_json.len() > MAX_UI_JSON_BYTES {
            return Err(WorkshopUiError::TooLarge);
        }
        if self
            .current_cancel
            .lock()
            .expect("workshop UI cancel")
            .is_some()
        {
            return Err(WorkshopUiError::Busy);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            experiment: String,
            cases: Vec<EvaluationCase>,
        }
        let request: Request = serde_json::from_str(request_json)
            .map_err(|error| WorkshopUiError::Invalid(error.to_string()))?;
        if request.experiment.trim().is_empty() || request.experiment.len() > 256 {
            return Err(WorkshopUiError::Invalid(
                "bounded experiment name required".into(),
            ));
        }
        if request.cases.len() < 2 || request.cases.len() > MAX_CASES {
            return Err(WorkshopUiError::Invalid(
                "at least two protected cases required".into(),
            ));
        }
        self.workshop
            .protect_cases(&request.experiment, &request.cases)?;
        Ok(())
    }

    /// Parse only the candidate schema and require the host-assigned scope.
    pub fn submit_candidate_json(
        &self,
        experiment: &str,
        assigned_scope: &Scope,
        candidate_json: &str,
    ) -> Result<(), WorkshopUiError> {
        if candidate_json.len() > MAX_UI_JSON_BYTES {
            return Err(WorkshopUiError::TooLarge);
        }
        if experiment.trim().is_empty() || experiment.len() > 256 {
            return Err(WorkshopUiError::Invalid(
                "bounded experiment name required".into(),
            ));
        }
        let raw: serde_json::Value = serde_json::from_str(candidate_json)
            .map_err(|error| WorkshopUiError::Invalid(error.to_string()))?;
        let object = raw
            .as_object()
            .ok_or_else(|| WorkshopUiError::Invalid("candidate object required".into()))?;
        if object
            .keys()
            .any(|key| key != "definition" && key != "authoring_evidence")
        {
            return Err(WorkshopUiError::Invalid(
                "unknown candidate fields are rejected".into(),
            ));
        }
        let candidate: CandidateManifest = serde_json::from_value(raw)
            .map_err(|error| WorkshopUiError::Invalid(error.to_string()))?;
        if candidate.definition.input_scope != *assigned_scope {
            return Err(WorkshopUiError::Invalid(
                "candidate scope does not exactly match assigned scope".into(),
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut current = self.current_cancel.lock().expect("workshop UI cancel");
            if current.is_some() {
                return Err(WorkshopUiError::Busy);
            }
            *current = Some(cancel.clone());
        }
        set_snapshot(
            &self.snapshot,
            UiSnapshot {
                state: UiState::Running,
                report: None,
                error: None,
            },
        );
        if let Err(error) = self.jobs.try_send(CandidateJob {
            experiment: experiment.into(),
            candidate,
            cancel,
        }) {
            *self.current_cancel.lock().expect("workshop UI cancel") = None;
            set_snapshot(
                &self.snapshot,
                UiSnapshot {
                    state: UiState::Failed,
                    report: None,
                    error: Some(error.to_string()),
                },
            );
            return Err(match error {
                mpsc::TrySendError::Full(_) => WorkshopUiError::Busy,
                mpsc::TrySendError::Disconnected(_) => WorkshopUiError::Closed,
            });
        }
        Ok(())
    }

    pub fn approve_exact(
        &self,
        hash: &str,
        scope: Scope,
        expiry: f64,
    ) -> Result<String, WorkshopUiError> {
        Ok(self.workshop.approve_exact(hash, scope, expiry)?)
    }
    pub fn revoke(&self, grant_id: &str) -> Result<(), WorkshopUiError> {
        Ok(self.workshop.revoke(grant_id)?)
    }
    pub fn rollback_exact(
        &self,
        name: &str,
        hash: &str,
        scope: Scope,
        expiry: f64,
    ) -> Result<String, WorkshopUiError> {
        Ok(self.workshop.rollback_exact(name, hash, scope, expiry)?)
    }
    pub fn invoke(
        &self,
        name: &str,
        inputs: &[ScopedInput],
    ) -> Result<ToolResult, WorkshopUiError> {
        Ok(self.workshop.invoke(name, inputs, None)?)
    }

    pub fn is_active_hash(&self, hash: &str, scope: Scope) -> Result<bool, WorkshopUiError> {
        Ok(self.workshop.is_active_hash(hash, scope)?)
    }

    pub fn cancel(&self) -> Result<(), WorkshopUiError> {
        let current = self
            .current_cancel
            .lock()
            .expect("workshop UI cancel")
            .clone();
        current
            .map(|flag| {
                flag.store(true, Ordering::Release);
            })
            .ok_or(WorkshopUiError::Busy)
    }
}

fn set_snapshot(cell: &Arc<RwLock<UiSnapshot>>, value: UiSnapshot) {
    *cell.write().expect("workshop UI snapshot") = value;
}

impl Drop for WorkshopUi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(cancel) = self
            .current_cancel
            .lock()
            .expect("workshop UI cancel")
            .clone()
        {
            cancel.store(true, Ordering::Release);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_evolution::{Expr, ToolDefinition};
    use serde_json::json;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn candidate_needs_explicit_approval_and_scope_is_exact() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private/workshop.sqlite");
        let scope = Scope::new(["personal"]);
        let inputs = |value| {
            vec![ScopedInput {
                value,
                scope: scope.clone(),
            }]
        };
        let ui = WorkshopUi::open(&path).unwrap();
        let cases = json!({
            "experiment": "titles",
            "cases": [
                { "inputs": inputs(json!({"title":"First"})), "expected": ["First"] },
                { "inputs": inputs(json!({"other":"missing"})), "expected": [null] }
            ]
        });
        ui.protect_cases_json(&cases.to_string()).unwrap();
        let candidate = CandidateManifest {
            definition: ToolDefinition {
                name: "titles".into(),
                version: 1,
                input_scope: scope.clone(),
                expression: Expr::Map {
                    input: Box::new(Expr::Input),
                    expr: Box::new(Expr::CurrentField {
                        path: "title".into(),
                    }),
                },
            },
            authoring_evidence: "candidate fixture".into(),
        };
        let candidate_json = serde_json::to_string(&candidate).unwrap();
        assert!(
            ui.submit_candidate_json("titles", &Scope::new(["other"]), &candidate_json)
                .is_err()
        );
        ui.submit_candidate_json("titles", &scope, &candidate_json)
            .unwrap();
        for _ in 0..100 {
            if matches!(ui.snapshot().state, UiState::Complete | UiState::Failed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let report = ui.snapshot().report.expect("evaluation report");
        assert!(report.passed);
        assert!(
            ui.invoke("titles", &inputs(json!({"title":"Fresh"})))
                .is_err()
        );
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 3600.0;
        ui.approve_exact(&report.tool_hash, scope.clone(), expiry)
            .unwrap();
        assert_eq!(
            ui.invoke("titles", &inputs(json!({"title":"Fresh"})))
                .unwrap()
                .value,
            json!(["Fresh"])
        );
    }

    #[test]
    fn protected_suite_requires_two_cases_and_input_is_bounded() {
        let temporary = tempfile::tempdir().unwrap();
        let ui = WorkshopUi::open(temporary.path().join("private/workshop.sqlite")).unwrap();
        let one = serde_json::json!({"experiment":"x","cases":[{"inputs":[],"expected":null}]});
        assert!(ui.protect_cases_json(&one.to_string()).is_err());
        assert!(
            ui.protect_cases_json(&"x".repeat(MAX_UI_JSON_BYTES + 1))
                .is_err()
        );
    }
}
