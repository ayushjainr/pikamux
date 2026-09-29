//! Evaluated working methods share the tool workshop and restricted interpreter.
//! Their output is bounded advisory working instructions, never permissions,
//! provider calls or execution commands. Legacy step names remain readable.
use crate::assistant_evolution::{
    CandidateManifest, EvaluationCase, EvaluationReport, EvolutionError, Scope as ToolScope,
    ScopedInput, ToolResult,
};
use crate::assistant_memory::{MemoryError, Scope, Store};
use crate::assistant_workshop::Workshop;
use crate::assistant_workshop_handoff::{self, ProposalKind};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MethodAction {
    ReviewEvidence,
    CheckContraryEvidence,
    CompareAlternatives,
    StateUncertainty,
    ClarifyMaterialGap,
    PreserveCommitments,
    DistinguishDecisionFromExecution,
    #[serde(untagged)]
    Instruction(String),
}
impl MethodAction {
    pub(crate) fn instruction(&self) -> &str {
        match self {
            Self::Instruction(text) => text,
            Self::ReviewEvidence => {
                "Check available permitted evidence before drawing a conclusion."
            }
            Self::CheckContraryEvidence => {
                "Consider relevant contrary evidence before finalizing the conclusion."
            }
            Self::CompareAlternatives => {
                "Compare relevant alternatives and explain the material tradeoff."
            }
            Self::StateUncertainty => {
                "Distinguish what is known from inference and state material uncertainty."
            }
            Self::ClarifyMaterialGap => {
                "Ask a focused question when a material missing fact prevents a responsible answer."
            }
            Self::PreserveCommitments => {
                "Check relevant outstanding commitments without claiming they are completed."
            }
            Self::DistinguishDecisionFromExecution => {
                "Distinguish discussion, accepted decisions, planned actions, and receipt-confirmed execution."
            }
        }
    }
}

/// Trusted host cases, never deserialized from the author response. Every role
/// is mandatory; cases are fixed before a candidate is evaluated.
pub(crate) struct MethodCases {
    pub original_failure: Vec<EvaluationCase>,
    pub contrasting: Vec<EvaluationCase>,
    pub protected: Vec<EvaluationCase>,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum MethodError {
    #[error("method workshop: {0}")]
    Evolution(#[from] EvolutionError),
    #[error("method memory: {0}")]
    Memory(#[from] MemoryError),
    #[error("method database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("method serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid method: {0}")]
    Invalid(String),
}
pub(crate) struct MethodResult {
    pub actions: Vec<MethodAction>,
    pub receipt: ToolResult,
}
pub(crate) struct MethodWorkshop {
    workshop: Workshop,
    memory_path: PathBuf,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MethodGuidance {
    pub hash: String,
    pub proposal_id: String,
    pub sources: Vec<crate::assistant_context::SourceVersion>,
    pub instructions: Vec<String>,
}

fn schema(memory: &Store) -> Result<(), MethodError> {
    memory.connection.execute_batch("CREATE TABLE IF NOT EXISTS method_suites(experiment TEXT PRIMARY KEY); CREATE TABLE IF NOT EXISTS method_versions(hash TEXT PRIMARY KEY,proposal_id TEXT NOT NULL,name TEXT NOT NULL,scope_json TEXT NOT NULL)")?;
    // Initialize epoch zero before installing the update trigger, so the first
    // forget is covered too. Install only after every referenced table exists.
    // Stable hash/proposal tombstones live in workshop_handoffs and are retained.
    memory.connection.execute_batch("INSERT OR IGNORE INTO memory_meta(key,value) VALUES('forget_epoch','0');
        DELETE FROM method_suites WHERE COALESCE((SELECT value FROM memory_meta WHERE key='method_metadata_epoch'),'-1') != (SELECT value FROM memory_meta WHERE key='forget_epoch') AND (SELECT value FROM memory_meta WHERE key='forget_epoch')!='0';
        DELETE FROM method_versions WHERE COALESCE((SELECT value FROM memory_meta WHERE key='method_metadata_epoch'),'-1') != (SELECT value FROM memory_meta WHERE key='forget_epoch') AND (SELECT value FROM memory_meta WHERE key='forget_epoch')!='0';
        INSERT INTO memory_meta(key,value) SELECT 'method_metadata_epoch',value FROM memory_meta WHERE key='forget_epoch' ON CONFLICT(key) DO UPDATE SET value=excluded.value;
        CREATE TRIGGER IF NOT EXISTS method_metadata_forget_scrub AFTER UPDATE ON memory_meta WHEN NEW.key='forget_epoch' AND NEW.value!=OLD.value BEGIN DELETE FROM method_suites; DELETE FROM method_versions; UPDATE memory_meta SET value=NEW.value WHERE key='method_metadata_epoch'; END;")?;
    Ok(())
}
fn actions(value: &Value) -> Result<Vec<MethodAction>, MethodError> {
    let parsed: Vec<MethodAction> = serde_json::from_value(value.clone()).map_err(|_| {
        MethodError::Invalid("method output must be an array of plain-English working steps".into())
    })?;
    if parsed.len() > 8
        || parsed
            .iter()
            .any(|a| a.instruction().trim().is_empty() || a.instruction().len() > 2048)
    {
        return Err(MethodError::Invalid(
            "method output needs at most eight nonempty steps of at most 2048 bytes".into(),
        ));
    }
    let keys = parsed
        .iter()
        .map(|a| a.instruction())
        .collect::<BTreeSet<_>>();
    if keys.len() != parsed.len() {
        return Err(MethodError::Invalid(
            "method repeats an interaction step".into(),
        ));
    }
    Ok(parsed)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MethodInput {
    queries: Vec<String>,
}
fn validate_input(value: &Value) -> Result<(), MethodError> {
    let input: MethodInput = serde_json::from_value(value.clone())
        .map_err(|_| MethodError::Invalid("method input must be the host queries array".into()))?;
    if input.queries.len() > 4 || input.queries.iter().any(|q| q.len() > 4096) {
        return Err(MethodError::Invalid(
            "method query input exceeds context bounds".into(),
        ));
    }
    Ok(())
}
fn tool_scope(scope: &Scope) -> Result<ToolScope, MethodError> {
    if scope.node.is_some() || scope.provider.is_some() || scope.conversation.is_some() {
        return Err(MethodError::Invalid(
            "method runner cannot broaden exact source scope".into(),
        ));
    }
    let project = scope
        .project
        .as_ref()
        .ok_or_else(|| MethodError::Invalid("method requires an exact project scope".into()))?;
    Ok(ToolScope::new([project.clone()]))
}

fn checked_cases(experiment: &str, cases: MethodCases) -> Result<Vec<EvaluationCase>, MethodError> {
    if experiment.is_empty()
        || experiment.len() > 128
        || cases.original_failure.is_empty()
        || cases.contrasting.is_empty()
        || cases.protected.is_empty()
    {
        return Err(MethodError::Invalid(
            "original, contrasting, and protected cases are all required".into(),
        ));
    }
    let mut combined = Vec::new();
    let mut inputs = BTreeSet::new();
    for case in cases
        .original_failure
        .into_iter()
        .chain(cases.contrasting)
        .chain(cases.protected)
    {
        check_case(&case, &mut inputs)?;
        combined.push(case);
    }
    if combined.len() > 16 {
        return Err(MethodError::Invalid("method cases exceed bound".into()));
    }
    Ok(combined)
}
fn check_case(case: &EvaluationCase, inputs: &mut BTreeSet<String>) -> Result<(), MethodError> {
    if case.inputs.len() != 1 {
        return Err(MethodError::Invalid(
            "method cases require one host-context input".into(),
        ));
    }
    validate_input(&case.inputs[0].value)?;
    actions(&case.expected)?;
    if !inputs.insert(serde_json::to_string(&case.inputs)?) {
        return Err(MethodError::Invalid(
            "method case roles require distinct supplied inputs".into(),
        ));
    }
    Ok(())
}
fn require_suite(memory: &Store, experiment: &str) -> Result<(), MethodError> {
    let ready: bool = memory.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM method_suites WHERE experiment=?)",
        [experiment],
        |r| r.get(0),
    )?;
    if !ready {
        return Err(MethodError::Invalid(
            "protect all case roles before evaluating a method".into(),
        ));
    }
    Ok(())
}
fn candidate_scope(
    memory: &Store,
    proposal_id: &str,
    candidate: &CandidateManifest,
) -> Result<Scope, MethodError> {
    let (scope, proposal) = assistant_workshop_handoff::proposal(memory, proposal_id)?;
    if !matches!(proposal.kind, ProposalKind::Method)
        || candidate.definition.input_scope != tool_scope(&scope)?
        || !candidate.definition.name.starts_with("method:")
        || proposal
            .requested_capabilities
            .iter()
            .any(|c| c != "interaction_guidance")
    {
        return Err(MethodError::Invalid(
            "method kind, scope, name, or capability mismatch".into(),
        ));
    }
    Ok(scope)
}
fn register_version(
    memory: &Store,
    proposal_id: &str,
    hash: &str,
    name: &str,
    scope: &Scope,
) -> Result<(), MethodError> {
    let existing: Option<String> = memory
        .connection
        .query_row(
            "SELECT proposal_id FROM method_versions WHERE hash=?",
            [hash],
            |r| r.get(0),
        )
        .optional()?;
    if existing.as_deref().is_some_and(|id| id != proposal_id) {
        return Err(MethodError::Invalid(
            "method version already belongs to another proposal".into(),
        ));
    }
    memory.connection.execute(
        "INSERT OR IGNORE INTO method_versions(hash,proposal_id,name,scope_json) VALUES(?,?,?,?)",
        params![hash, proposal_id, name, serde_json::to_string(scope)?],
    )?;
    Ok(())
}

impl MethodWorkshop {
    pub(crate) fn open(path: &Path) -> Result<Self, MethodError> {
        Ok(Self {
            workshop: Workshop::open(path)?,
            memory_path: path.with_file_name("memory.sqlite"),
        })
    }
    fn same_memory(&self, memory: &Store) -> Result<(), MethodError> {
        if memory.path() != self.memory_path {
            return Err(MethodError::Invalid(
                "method belongs to a different authority".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn protect_cases(
        &self,
        memory: &Store,
        experiment: &str,
        cases: MethodCases,
    ) -> Result<(), MethodError> {
        self.same_memory(memory)?;
        let combined = checked_cases(experiment, cases)?;
        self.workshop
            .protect_cases(&format!("method:{experiment}"), &combined)?;
        schema(memory)?;
        memory.connection.execute(
            "INSERT OR IGNORE INTO method_suites(experiment) VALUES(?)",
            [experiment],
        )?;
        Ok(())
    }

    /// The author/model call, if any, must already have been admitted under P05.
    /// This path performs only native evaluation and cannot issue an approval.
    pub(crate) fn submit(
        &self,
        memory: &mut Store,
        proposal_id: &str,
        experiment: &str,
        candidate: &CandidateManifest,
        cancel: Option<&AtomicBool>,
    ) -> Result<EvaluationReport, MethodError> {
        self.same_memory(memory)?;
        schema(memory)?;
        require_suite(memory, experiment)?;
        let scope = candidate_scope(memory, proposal_id, candidate)?;
        let hash = crate::assistant_evolution::validate_definition(&candidate.definition)?;
        assistant_workshop_handoff::bind_candidate(memory, proposal_id, &hash)?;
        register_version(
            memory,
            proposal_id,
            &hash,
            &candidate.definition.name,
            &scope,
        )?;
        Ok(self
            .workshop
            .submit_candidate(&format!("method:{experiment}"), candidate, cancel)?)
    }

    fn registered(&self, memory: &Store, hash: &str, scope: &Scope) -> Result<(), MethodError> {
        self.same_memory(memory)?;
        let id: Option<String> = memory
            .connection
            .query_row(
                "SELECT proposal_id FROM method_versions WHERE hash=? AND scope_json=?",
                params![hash, serde_json::to_string(scope)?],
                |r| r.get(0),
            )
            .optional()?;
        let id = id.ok_or_else(|| {
            MethodError::Invalid("exact evaluated method version not found".into())
        })?;
        let (stored, proposal) = assistant_workshop_handoff::proposal(memory, &id)?;
        if stored != *scope || !matches!(proposal.kind, ProposalKind::Method) {
            return Err(MethodError::Invalid(
                "method source is not valid for this scope".into(),
            ));
        }
        Ok(())
    }

    /// Only explicit human approval or a separately checked standing grant may
    /// call this. Generated findings do not get a path to activation.
    pub(crate) fn approve_exact(
        &self,
        memory: &Store,
        scope: &Scope,
        hash: &str,
    ) -> Result<String, MethodError> {
        self.registered(memory, hash, scope)?;
        Ok(self.workshop.approve_exact(hash, tool_scope(scope)?)?)
    }

    pub(crate) fn invoke(
        &self,
        memory: &Store,
        scope: &Scope,
        name: &str,
        input: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<MethodResult, MethodError> {
        validate_input(&input)?;
        let catalog = self.workshop.catalog(&tool_scope(scope)?, None)?;
        let hash = catalog["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["active"] == true && t["name"] == name)
            .and_then(|t| t["hash"].as_str())
            .ok_or_else(|| MethodError::Invalid("method is not active".into()))?;
        self.registered(memory, hash, scope)?;
        if serde_json::to_vec(&input)?.len() > 32 * 1024 {
            return Err(MethodError::Invalid(
                "method fresh input exceeds bound".into(),
            ));
        }
        let receipt = self.workshop.invoke(
            name,
            &[ScopedInput {
                value: input,
                scope: tool_scope(scope)?,
            }],
            cancel,
        )?;
        let actions = match actions(&receipt.value) {
            Ok(actions) => actions,
            Err(error) => {
                // Deterministic output-contract failure is evidence of regression,
                // not a human benefit assessment. Disable through existing grant
                // revocation; never return generated free-form prompt text.
                self.workshop.reject_method_output(&receipt.tool_hash)?;
                return Err(error);
            }
        };
        self.registered(memory, &receipt.tool_hash, scope)?;
        Ok(MethodResult { actions, receipt })
    }

    pub(crate) fn assess(
        &self,
        scope: &Scope,
        hash: &str,
        outcome: &str,
        evidence: &str,
        rollback: Option<&str>,
    ) -> Result<Value, MethodError> {
        Ok(self
            .workshop
            .assess(&tool_scope(scope)?, hash, outcome, evidence, rollback)?)
    }
}

/// Context consumer: execute only already-approved pure methods against supplied
/// current scoped data. No method archive, provider call, or new permission.
fn method_guidance(memory: &Store, run: MethodResult) -> Result<MethodGuidance, MethodError> {
    let id: String = memory.connection.query_row(
        "SELECT proposal_id FROM method_versions WHERE hash=?",
        [&run.receipt.tool_hash],
        |r| r.get(0),
    )?;
    let (_, proposal) = assistant_workshop_handoff::proposal(memory, &id)?;
    let mut sources = proposal.sources;
    let revision = memory
        .source_version(&id)?
        .ok_or_else(|| MethodError::Invalid("method proposal disappeared".into()))?;
    sources.push(crate::assistant_context::SourceVersion {
        id: id.clone(),
        revision,
    });
    Ok(MethodGuidance {
        hash: run.receipt.tool_hash,
        proposal_id: id,
        sources,
        instructions: run
            .actions
            .iter()
            .map(|a| a.instruction().to_string())
            .collect(),
    })
}

fn method_names(memory: &Store, scope: &Scope) -> Result<Vec<String>, MethodError> {
    let mut query = memory.connection.prepare(
        "SELECT DISTINCT name FROM method_versions WHERE scope_json=? ORDER BY name LIMIT 8",
    )?;
    Ok(query
        .query_map([serde_json::to_string(scope)?], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn applicable(
    memory: &Store,
    scope: &Scope,
    input: Value,
) -> Result<Vec<MethodGuidance>, MethodError> {
    let exists: bool = memory.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='method_versions' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(vec![]);
    }
    let workshop_path = memory.path().with_file_name("workshop.sqlite");
    if !workshop_path.exists() {
        return Ok(vec![]);
    }
    let workshop = MethodWorkshop::open(&workshop_path)?;
    let mut result = Vec::new();
    for name in method_names(memory, scope)? {
        match workshop.invoke(memory, scope, &name, input.clone(), None) {
            Ok(run) => {
                result.push(method_guidance(memory, run)?);
            }
            Err(
                MethodError::Invalid(_)
                | MethodError::Evolution(
                    EvolutionError::NotFound(_)
                    | EvolutionError::NotEligible(_)
                    | EvolutionError::ApprovalMismatch,
                ),
            ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_evolution::{Expr, ToolDefinition};
    use crate::assistant_memory::{NewRecord, Origin, RecordKind};
    use crate::assistant_workshop_handoff::WorkshopProposal;

    struct Fixture {
        _temp: tempfile::TempDir,
        memory: Store,
        workshop: MethodWorkshop,
        scope: Scope,
        proposal: String,
        source: String,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("private");
            let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
            let scope = Scope {
                project: Some("fixture".into()),
                ..Scope::default()
            };
            let source = memory
                .append(NewRecord {
                    kind: RecordKind::Finding,
                    origin: Origin::Human,
                    scope: scope.clone(),
                    body: "The last answer failed to distinguish inference from evidence".into(),
                    provenance: "synthetic observation".into(),
                    timestamp: 1,
                    supersedes: None,
                    dependencies: vec![],
                    decision_state: None,
                    protected_policy: false,
                })
                .unwrap();
            let proposal = WorkshopProposal {
                kind: ProposalKind::Method, hypothesis: "Explicit uncertainty review may avoid unsupported certainty".into(),
                proposed_change: "Select uncertainty review on uncertain inputs".into(), baseline: None,
                requested_capabilities: vec!["interaction_guidance".into()],
                success_criterion: "Original failure selects uncertainty review; contrasting and protected cases remain unchanged".into(),
                sources: vec![crate::assistant_context::SourceVersion { id: source.id.clone(), revision: memory.source_version(&source.id).unwrap().unwrap() }],
            };
            let sources = proposal.sources.clone();
            let envelope=crate::assistant_continuity::decode(&serde_json::json!({"pika_turn":1,"answer":"I will preserve that uncertainty; a method remains proposed pending protected evaluation.","learning":[{"kind":"workshop","proposal":proposal}]}).to_string()).unwrap().unwrap();
            let epoch = memory.forget_epoch().unwrap();
            let committed = crate::assistant_continuity::commit_turn(
                &mut memory,
                "ordinary-method-turn",
                NewRecord {
                    kind: RecordKind::Finding,
                    origin: Origin::Worker,
                    scope: scope.clone(),
                    body: envelope.answer,
                    provenance: "ordinary bounded synthetic response".into(),
                    timestamp: 2,
                    supersedes: None,
                    dependencies: vec![source.id.clone()],
                    decision_state: None,
                    protected_policy: false,
                },
                &envelope.learning,
                &sources,
                epoch,
            )
            .unwrap();
            let proposal = committed
                .into_iter()
                .find(|r| r.kind == RecordKind::Proposal)
                .unwrap();
            let workshop = MethodWorkshop::open(&root.join("workshop.sqlite")).unwrap();
            Self {
                _temp: temp,
                memory,
                workshop,
                scope,
                proposal: proposal.id,
                source: source.id,
            }
        }
        fn case(&self, input: Value, expected: Value) -> EvaluationCase {
            EvaluationCase {
                inputs: vec![ScopedInput {
                    value: input,
                    scope: tool_scope(&self.scope).unwrap(),
                }],
                expected,
            }
        }
        fn protect(&self) {
            self.workshop
                .protect_cases(
                    &self.memory,
                    "uncertainty",
                    MethodCases {
                        original_failure: vec![self.case(
                            serde_json::json!({"queries":["What supports that conclusion?"]}),
                            serde_json::json!(["state_uncertainty"]),
                        )],
                        contrasting: vec![self.case(
                            serde_json::json!({"queries":["What is the known status?"]}),
                            serde_json::json!(["state_uncertainty"]),
                        )],
                        protected: vec![
                            self.case(serde_json::json!({"queries":[]}), serde_json::json!([])),
                        ],
                    },
                )
                .unwrap();
        }
        fn candidate(&self, version: u64, broken: bool) -> CandidateManifest {
            CandidateManifest {
                definition: ToolDefinition {
                    name: "method:uncertainty-fixture".into(),
                    version,
                    input_scope: tool_scope(&self.scope).unwrap(),
                    expression: if broken {
                        Expr::Literal {
                            value: serde_json::json!([]),
                        }
                    } else {
                        Expr::Map {
                            input: Box::new(Expr::Filter {
                                input: Box::new(Expr::Input),
                                predicate: Box::new(Expr::Compare {
                                    comparison: crate::assistant_evolution::CompareOp::Ne,
                                    left: Box::new(Expr::CurrentField {
                                        path: "queries".into(),
                                    }),
                                    right: Box::new(Expr::Literal {
                                        value: serde_json::json!([]),
                                    }),
                                }),
                            }),
                            expr: Box::new(Expr::Literal {
                                value: serde_json::json!("state_uncertainty"),
                            }),
                        }
                    },
                },
                authoring_evidence: "Synthetic deterministic candidate; no model authoring call"
                    .into(),
            }
        }
    }

    #[test]
    fn methods_need_all_cases_exact_approval_and_fresh_use_is_source_fenced() {
        let mut f = Fixture::new();
        let candidate = f.candidate(1, false);
        assert!(
            f.workshop
                .submit(&mut f.memory, &f.proposal, "uncertainty", &candidate, None)
                .is_err()
        );
        f.protect();
        let report = f
            .workshop
            .submit(&mut f.memory, &f.proposal, "uncertainty", &candidate, None)
            .unwrap();
        assert!(report.passed);
        assert!(
            applicable(
                &f.memory,
                &f.scope,
                serde_json::json!({"queries":["A question"]})
            )
            .unwrap()
            .is_empty()
        );
        f.workshop
            .approve_exact(&f.memory, &f.scope, &report.tool_hash)
            .unwrap();
        let fresh = applicable(
            &f.memory,
            &f.scope,
            serde_json::json!({"queries":["A fresh question after restart"]}),
        )
        .unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(
            fresh[0].instructions,
            vec![MethodAction::StateUncertainty.instruction()]
        );
        assert!(fresh[0].sources.iter().any(|s| s.id == f.source));
        assert!(fresh[0].sources.iter().any(|s| s.id == f.proposal));
        let package = crate::assistant_context::build(
            &f.memory,
            &f.scope,
            &["A new ordinary question".into()],
            &[],
            32 * 1024,
        )
        .unwrap();
        assert_eq!(package.methods.len(), 1);
        assert_eq!(package.methods[0].hash, report.tool_hash);
        assert!(package.sources.iter().any(|s| s.id == f.proposal));
        f.memory.forget(&f.source).unwrap();
        assert_eq!(
            f.memory
                .connection
                .query_row("SELECT COUNT(*) FROM method_versions", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            f.memory
                .connection
                .query_row("SELECT COUNT(*) FROM method_suites", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            f.memory
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM workshop_handoffs WHERE candidate_hash IS NOT NULL",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            1
        );
        let reopened = Store::open(f.memory.path()).unwrap();
        assert!(
            applicable(
                &reopened,
                &f.scope,
                serde_json::json!({"queries":["A question"]})
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            f.workshop
                .approve_exact(&f.memory, &f.scope, &report.tool_hash)
                .is_err()
        );
    }

    #[test]
    fn a_bad_method_fails_protected_evaluation_and_cannot_be_approved() {
        let mut f = Fixture::new();
        f.protect();
        let candidate = f.candidate(1, true);
        let report = f
            .workshop
            .submit(&mut f.memory, &f.proposal, "uncertainty", &candidate, None)
            .unwrap();
        assert!(!report.passed);
        assert!(
            f.workshop
                .approve_exact(&f.memory, &f.scope, &report.tool_hash)
                .is_err()
        );
        assert!(
            f.workshop
                .protect_cases(
                    &f.memory,
                    "uncertainty",
                    MethodCases {
                        original_failure: vec![f.case(
                            serde_json::json!({"queries":["What supports that conclusion?"]}),
                            serde_json::json!([])
                        )],
                        contrasting: vec![f.case(
                            serde_json::json!({"queries":["What is the known status?"]}),
                            serde_json::json!([])
                        )],
                        protected: vec![
                            f.case(serde_json::json!({"queries":[]}), serde_json::json!([]))
                        ],
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn fresh_structured_action_is_not_advice_and_retires_version() {
        let mut f = Fixture::new();
        f.workshop
            .protect_cases(
                &f.memory,
                "dynamic",
                MethodCases {
                    original_failure: vec![f.case(
                        serde_json::json!({"queries":["original"]}),
                        serde_json::json!([]),
                    )],
                    contrasting: vec![f.case(
                        serde_json::json!({"queries":["contrast"]}),
                        serde_json::json!([]),
                    )],
                    protected: vec![
                        f.case(serde_json::json!({"queries":[]}), serde_json::json!([])),
                    ],
                },
            )
            .unwrap();
        let mut candidate = f.candidate(1, false);
        candidate.definition.expression = Expr::Map {
            input: Box::new(Expr::Filter {
                input: Box::new(Expr::Input),
                predicate: Box::new(Expr::Compare {
                    comparison: crate::assistant_evolution::CompareOp::Eq,
                    left: Box::new(Expr::CurrentField {
                        path: "queries".into(),
                    }),
                    right: Box::new(Expr::Literal {
                        value: serde_json::json!(["trigger"]),
                    }),
                }),
            }),
            expr: Box::new(Expr::Literal {
                value: serde_json::json!({"shell":"run a command"}),
            }),
        };
        let report = f
            .workshop
            .submit(&mut f.memory, &f.proposal, "dynamic", &candidate, None)
            .unwrap();
        assert!(report.passed);
        f.workshop
            .approve_exact(&f.memory, &f.scope, &report.tool_hash)
            .unwrap();
        assert!(
            f.workshop
                .invoke(
                    &f.memory,
                    &f.scope,
                    &candidate.definition.name,
                    serde_json::json!({"queries":["trigger"]}),
                    None
                )
                .is_err()
        );
        assert!(
            f.workshop
                .approve_exact(&f.memory, &f.scope, &report.tool_hash)
                .is_err()
        );
        let db =
            rusqlite::Connection::open(f.memory.path().with_file_name("workshop.sqlite")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM method_contract_failures", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
            1
        );
    }

    #[test]
    fn methods_accept_plain_english_working_steps_without_executable_actions() {
        let advice = actions(&serde_json::json!([
            "Sketch the smallest useful block diagram before explaining the architecture.",
            "review_evidence"
        ]))
        .unwrap();
        assert_eq!(advice.len(), 2);
        assert!(advice[0].instruction().starts_with("Sketch the smallest"));
        assert_eq!(advice[1], MethodAction::ReviewEvidence);
        assert!(actions(&serde_json::json!([{"shell":"anything"}])).is_err());
        assert!(actions(&serde_json::json!([""])).is_err());
        assert!(actions(&serde_json::json!(["same", "same"])).is_err());
        assert!(actions(&serde_json::json!(["x".repeat(2049)])).is_err());
    }
}
