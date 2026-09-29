//! Trusted host boundary around model-authored pure transformations.
//!
//! A provider can submit a candidate, not a test suite or an approval. The host
//! owns contrasting cases and exact-version human approval. Generated ASTs
//! receive only scoped JSON inputs, never a reference to this service.
use crate::assistant_evolution::{
    CandidateManifest, ComparisonReceipt, EvaluationCase, EvaluationReport, EvolutionError,
    Registry, Scope, ScopedInput, ToolResult,
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub struct Workshop {
    registry: Registry,
    memory_path: PathBuf,
}

impl Workshop {
    pub(crate) fn reject_method_output(&self, hash: &str) -> Result<(), EvolutionError> {
        self.registry.reject_method_output(hash)
    }
    pub fn catalog(
        &self,
        scope: &Scope,
        hash: Option<&str>,
    ) -> Result<serde_json::Value, EvolutionError> {
        let mut catalog = self.registry.catalog(scope, hash)?;
        if let Some(tools) = catalog["tools"].as_array_mut() {
            for tool in tools {
                if let Some(hash) = tool["hash"].as_str().map(str::to_owned) {
                    if self.validate_candidate_sources(&hash).is_err() {
                        *tool = serde_json::json!({"hash":hash,"active":false,"source_unavailable":true});
                    }
                }
            }
        }
        Ok(catalog)
    }

    /// Small context references only. This grants neither invocation nor new
    /// input access and never loads the reflection archive into a model turn.
    pub(crate) fn active_references(
        &self,
        scope: &Scope,
    ) -> Result<serde_json::Value, EvolutionError> {
        let catalog = self.catalog(scope, None)?;
        let references = catalog["tools"].as_array().into_iter().flatten()
            .filter(|tool| tool["active"] == true)
            .take(16)
            .map(|tool| serde_json::json!({"hash":tool["hash"],"name":tool["name"],"version":tool["version"],"scope":tool["scope"]}))
            .collect::<Vec<_>>();
        Ok(
            serde_json::json!({"tools":references,"truncated":catalog["truncated"],"notice":"References only. Invocation requires explicit scoped supplied data through the restricted runner."}),
        )
    }

    /// A rollback hash is an explicit human approval of that exact version,
    /// not an automatic inference from a negative assessment. Retirement is
    /// committed first, so a failed rollback leaves the regressed tool disabled.
    pub fn assess(
        &self,
        scope: &Scope,
        hash: &str,
        outcome: &str,
        evidence: &str,
        rollback: Option<&str>,
    ) -> Result<serde_json::Value, EvolutionError> {
        if rollback.is_some() && !matches!(outcome, "regression" | "retire") {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let candidate = self
            .registry
            .candidate(hash)?
            .ok_or_else(|| EvolutionError::NotFound(hash.into()))?;
        self.validate_assessment_rollback(&candidate.definition.name, scope, hash, rollback)?;
        self.registry.assess(hash, scope, outcome, evidence)?;
        let mut receipt = serde_json::json!({"hash":hash,"outcome":outcome,"retired":matches!(outcome,"regression"|"retire"),"human_assessment":true});
        if let Some(target) = rollback {
            match self.rollback_exact(&candidate.definition.name, target, scope.clone()) {
                Ok(grant) => {
                    receipt["rollback"] =
                        serde_json::json!({"hash":target,"grant_id":grant,"completed":true})
                }
                Err(error) => {
                    receipt["rollback"] = serde_json::json!({"hash":target,"completed":false,"error":error.to_string(),"notice":"Assessed version remains retired; no rollback was claimed."})
                }
            }
        }
        Ok(receipt)
    }

    fn validate_assessment_rollback(
        &self,
        name: &str,
        scope: &Scope,
        assessed_hash: &str,
        rollback: Option<&str>,
    ) -> Result<(), EvolutionError> {
        if let Some(target) = rollback {
            let previous = self
                .registry
                .candidate(target)?
                .ok_or_else(|| EvolutionError::NotFound(target.into()))?;
            if target == assessed_hash
                || previous.definition.name != name
                || previous.definition.input_scope != *scope
            {
                return Err(EvolutionError::ApprovalMismatch);
            }
        }
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Self, EvolutionError> {
        Ok(Self {
            registry: Registry::open(path)?,
            memory_path: path.with_file_name("memory.sqlite"),
        })
    }

    /// Trusted evaluator input only. No candidate-defined test cases are used.
    pub fn protect_cases(
        &self,
        experiment: &str,
        cases: &[EvaluationCase],
    ) -> Result<(), EvolutionError> {
        self.registry.evaluator().protect_suite(experiment, cases)
    }

    /// The experiment is selected by the host assignment, not model output.
    /// A previously committed maintenance proposal can enter the same evaluator
    /// after a separately admitted authoring job. Its dependency binding precedes
    /// candidate publication and survives an interrupted evaluator.
    pub(crate) fn submit_handoff_candidate(
        &self,
        memory: &mut crate::assistant_memory::Store,
        proposal_id: &str,
        experiment: &str,
        candidate: &CandidateManifest,
        cancellation: Option<&AtomicBool>,
    ) -> Result<EvaluationReport, EvolutionError> {
        if memory.path() != self.memory_path {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let (scope, proposal) = crate::assistant_workshop_handoff::proposal(memory, proposal_id)
            .map_err(|e| EvolutionError::NotEligible(e.to_string()))?;
        if !matches!(
            proposal.kind,
            crate::assistant_workshop_handoff::ProposalKind::Tool
        ) || scope.node.is_some()
            || scope.provider.is_some()
            || scope.conversation.is_some()
            || scope.project.as_ref().is_none_or(|project| {
                candidate.definition.input_scope != Scope::new([project.clone()])
            })
            || proposal
                .requested_capabilities
                .iter()
                .any(|capability| capability != "supplied_json")
        {
            return Err(EvolutionError::NotEligible("proposal scope or requested capabilities cannot be enforced by this pure-data tool".into()));
        }
        let hash = crate::assistant_evolution::validate_definition(&candidate.definition)?;
        crate::assistant_workshop_handoff::bind_candidate(memory, proposal_id, &hash)
            .map_err(|e| EvolutionError::NotEligible(e.to_string()))?;
        self.validate_candidate_sources(&hash)?;
        self.submit_candidate(experiment, candidate, cancellation)
    }

    pub fn submit_candidate(
        &self,
        experiment: &str,
        candidate: &CandidateManifest,
        cancellation: Option<&AtomicBool>,
    ) -> Result<EvaluationReport, EvolutionError> {
        const MAX_INHERITED_SUITES: usize = 16;
        let baseline_hash = self.registry.active_baseline_hash(
            &candidate.definition.name,
            &candidate.definition.input_scope,
        )?;
        let hash = self.registry.author().register(candidate)?;
        let mut required = vec![experiment.to_owned()];
        if let Some(baseline_hash) = baseline_hash.as_deref() {
            required.extend(self.registry.required_suites(baseline_hash)?);
        }
        required.sort();
        required.dedup();
        if required.len() > MAX_INHERITED_SUITES {
            return Err(EvolutionError::Limit(
                "candidate inherits too many protected comparison suites".into(),
            ));
        }
        self.registry
            .evaluator()
            .designate_required(&hash, &required)?;
        let mut cases = 0;
        let mut failures = Vec::new();
        for suite in &required {
            let evaluated = self
                .registry
                .evaluator()
                .evaluate_suite(&hash, suite, cancellation)?;
            cases += evaluated.cases;
            failures.extend(
                evaluated
                    .failures
                    .into_iter()
                    .map(|failure| format!("{suite}: {failure}")),
            );
            self.registry
                .compare_candidate(suite, &hash, cancellation)?;
        }
        Ok(EvaluationReport {
            tool_hash: hash,
            passed: cases != 0 && failures.is_empty(),
            cases,
            failures,
        })
    }

    /// Call only from explicit local user approval of the displayed exact hash.
    /// Approval has no time expiry; revocation, retirement and source validity
    /// still apply. The provider response parser has no path to this method.
    pub fn approve_exact(
        &self,
        displayed_hash: &str,
        approved_scope: Scope,
    ) -> Result<String, EvolutionError> {
        self.validate_candidate_sources(displayed_hash)?;
        self.require_comparisons(displayed_hash)?;
        let grant = self
            .registry
            .policy()
            .issue_grant(displayed_hash, approved_scope, None)?;
        if let Err(error) = self
            .registry
            .policy()
            .activate(&grant)
            .and_then(|()| self.validate_candidate_sources(displayed_hash))
        {
            self.registry.policy().revoke(grant.id())?;
            return Err(error);
        }
        Ok(grant.id().to_owned())
    }

    pub fn rollback_exact(
        &self,
        name: &str,
        displayed_hash: &str,
        scope: Scope,
    ) -> Result<String, EvolutionError> {
        self.validate_candidate_sources(displayed_hash)?;
        self.require_comparisons(displayed_hash)?;
        let grant = self
            .registry
            .policy()
            .issue_grant(displayed_hash, scope, None)?;
        if let Err(error) = self
            .registry
            .policy()
            .rollback(name, &grant)
            .and_then(|()| self.validate_candidate_sources(displayed_hash))
        {
            self.registry.policy().revoke(grant.id())?;
            return Err(error);
        }
        Ok(grant.id().to_owned())
    }

    pub fn revoke(&self, grant_id: &str) -> Result<(), EvolutionError> {
        self.registry.policy().revoke(grant_id)
    }

    pub fn invoke(
        &self,
        name: &str,
        inputs: &[ScopedInput],
        cancel: Option<&AtomicBool>,
    ) -> Result<ToolResult, EvolutionError> {
        let (hash, _) = self.registry.active_tool_grant(name)?;
        self.validate_candidate_sources(&hash)?;
        let result = self.registry.invoke(name, inputs, cancel)?;
        self.validate_candidate_sources(&result.tool_hash)?;
        Ok(result)
    }

    pub fn is_active_hash(&self, hash: &str, scope: Scope) -> Result<bool, EvolutionError> {
        if self.validate_candidate_sources(hash).is_err() {
            return Ok(false);
        }
        self.registry.is_active_hash(hash, &scope)
    }

    /// Maintenance proposals remain authoritative memory, never copied into a
    /// tool archive. Dangling bindings intentionally fail closed after forgetting.
    fn validate_candidate_sources(&self, hash: &str) -> Result<(), EvolutionError> {
        if !self.memory_path.exists() {
            return Ok(());
        }
        let store = crate::assistant_memory::Store::open(&self.memory_path)
            .map_err(|e| EvolutionError::NotEligible(e.to_string()))?;
        crate::assistant_workshop_handoff::validate_candidate_sources(&store, hash)
            .map_err(|e| EvolutionError::NotEligible(e.to_string()))
    }

    pub fn latest_evaluation(
        &self,
        experiment: &str,
        candidate_hash: &str,
    ) -> Result<Option<EvaluationReport>, EvolutionError> {
        self.registry.latest_evaluation(experiment, candidate_hash)
    }

    pub fn latest_comparison(
        &self,
        experiment: &str,
        candidate_hash: &str,
    ) -> Result<Option<ComparisonReceipt>, EvolutionError> {
        self.registry.latest_comparison(experiment, candidate_hash)
    }

    fn require_comparisons(&self, hash: &str) -> Result<(), EvolutionError> {
        let suites = self.registry.required_suites(hash)?;
        if suites.is_empty() {
            return Err(EvolutionError::ApprovalMismatch);
        }
        for suite in suites {
            if self.registry.latest_comparison(&suite, hash)?.is_none() {
                return Err(EvolutionError::ApprovalMismatch);
            }
        }
        Ok(())
    }

    pub fn comparisons(
        &self,
        candidate_hash: &str,
    ) -> Result<Vec<ComparisonReceipt>, EvolutionError> {
        self.registry
            .required_suites(candidate_hash)?
            .iter()
            .map(|suite| self.registry.latest_comparison(suite, candidate_hash))
            .collect::<Result<Vec<_>, _>>()
            .map(|receipts| receipts.into_iter().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_evolution::{Expr, ToolDefinition};
    use serde_json::json;

    #[test]
    fn scoped_catalog_and_human_regression_rollback_survive_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private/workshop.sqlite");
        let scope = Scope::new(["alpha"]);
        let workshop = Workshop::open(&path).unwrap();
        let cases = [
            EvaluationCase {
                inputs: vec![ScopedInput {
                    value: json!("first"),
                    scope: scope.clone(),
                }],
                expected: json!(["first"]),
            },
            EvaluationCase {
                inputs: vec![],
                expected: json!([]),
            },
        ];
        workshop.protect_cases("identity", &cases).unwrap();
        let mut candidate = CandidateManifest {
            definition: ToolDefinition {
                name: "alpha-identity".into(),
                version: 1,
                input_scope: scope.clone(),
                expression: Expr::Input,
            },
            authoring_evidence: "fake isolated author".into(),
        };
        let first = workshop
            .submit_candidate("identity", &candidate, None)
            .unwrap();
        workshop
            .approve_exact(&first.tool_hash, scope.clone())
            .unwrap();
        candidate.definition.version = 2;
        let second = workshop
            .submit_candidate("identity", &candidate, None)
            .unwrap();
        workshop
            .approve_exact(&second.tool_hash, scope.clone())
            .unwrap();
        drop(workshop);
        let workshop = Workshop::open(&path).unwrap();
        assert_eq!(
            workshop.catalog(&scope, None).unwrap()["tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let detail = workshop.catalog(&scope, Some(&second.tool_hash)).unwrap();
        assert_eq!(detail["tools"][0]["definition"]["name"], "alpha-identity");
        assert_eq!(detail["tools"][0]["tests"][0]["evaluation"]["passed"], true);
        assert!(detail["tools"][0]["tests"][0]["comparison"]["candidate_fuel"].is_number());
        assert_eq!(
            workshop
                .catalog(&Scope::new(["beta"]), Some(&second.tool_hash))
                .unwrap()["tools"],
            json!([])
        );
        assert!(
            workshop
                .assess(
                    &Scope::new(["beta"]),
                    &second.tool_hash,
                    "regression",
                    "not my scope",
                    None,
                )
                .is_err()
        );
        let receipt = workshop
            .assess(
                &scope,
                &second.tool_hash,
                "regression",
                "Fresh case made the briefing less useful",
                Some(&first.tool_hash),
            )
            .unwrap();
        assert_eq!(receipt["rollback"]["completed"], true);
        assert!(
            workshop
                .approve_exact(&second.tool_hash, scope.clone())
                .is_err()
        );
        let fresh = [ScopedInput {
            value: json!("withheld"),
            scope: scope.clone(),
        }];
        assert_eq!(
            workshop
                .invoke("alpha-identity", &fresh, None)
                .unwrap()
                .tool_hash,
            first.tool_hash
        );
        drop(workshop);
        let workshop = Workshop::open(&path).unwrap();
        let detail = workshop.catalog(&scope, Some(&second.tool_hash)).unwrap();
        assert_eq!(detail["tools"][0]["retired"], true);
        let grants = detail["tools"][0]["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 1, "rejected approval must not issue a grant");
        assert!(grants.iter().all(|grant| grant["revoked"] == true));
        assert_eq!(
            detail["tools"][0]["assessments"][0]["outcome"],
            "regression"
        );
        workshop
            .assess(&scope, &first.tool_hash, "retire", "No longer needed", None)
            .unwrap();
        assert!(workshop.invoke("alpha-identity", &fresh, None).is_err());
        assert!(
            workshop
                .approve_exact(&first.tool_hash, scope.clone())
                .is_err()
        );
        let failed_rollback = workshop
            .assess(
                &scope,
                &second.tool_hash,
                "regression",
                "Still not useful",
                Some(&first.tool_hash),
            )
            .unwrap();
        assert_eq!(failed_rollback["rollback"]["completed"], false);
        assert!(workshop.invoke("alpha-identity", &fresh, None).is_err());
    }

    #[test]
    fn same_named_tool_cannot_replace_another_project_activation() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private/workshop.sqlite");
        let workshop = Workshop::open(&path).unwrap();
        let mut hashes = Vec::new();
        for project in ["alpha", "beta"] {
            let scope = Scope::new([project]);
            let cases = [EvaluationCase {
                inputs: vec![ScopedInput {
                    value: json!(project),
                    scope: scope.clone(),
                }],
                expected: json!([project]),
            }];
            workshop.protect_cases(project, &cases).unwrap();
            let candidate = CandidateManifest {
                definition: ToolDefinition {
                    name: "same-name".into(),
                    version: 1,
                    input_scope: scope.clone(),
                    expression: Expr::Input,
                },
                authoring_evidence: "fixture".into(),
            };
            let report = workshop
                .submit_candidate(project, &candidate, None)
                .unwrap();
            let activation = workshop.approve_exact(&report.tool_hash, scope);
            assert_eq!(activation.is_ok(), project == "alpha");
            hashes.push(report.tool_hash);
        }
        assert!(
            workshop
                .is_active_hash(&hashes[0], Scope::new(["alpha"]))
                .unwrap()
        );
        assert!(
            !workshop
                .is_active_hash(&hashes[1], Scope::new(["beta"]))
                .unwrap()
        );
        assert!(
            workshop
                .rollback_exact("wrong-name", &hashes[0], Scope::new(["alpha"]))
                .is_err()
        );
        drop(workshop);
        let workshop = Workshop::open(&path).unwrap();
        let rejected = workshop
            .catalog(&Scope::new(["beta"]), Some(&hashes[1]))
            .unwrap();
        let grants = rejected["tools"][0]["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 1);
        assert!(grants.iter().all(|grant| grant["revoked"] == true));
        let retained = workshop
            .catalog(&Scope::new(["alpha"]), Some(&hashes[0]))
            .unwrap();
        let grants = retained["tools"][0]["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 2);
        assert_eq!(
            grants
                .iter()
                .filter(|grant| grant["revoked"] == false)
                .count(),
            1
        );
        assert!(
            workshop
                .is_active_hash(&hashes[0], Scope::new(["alpha"]))
                .unwrap()
        );
    }

    #[test]
    fn authored_transform_requires_independent_cases_exact_approval_and_reuses_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private/workshop.sqlite");
        let scope = Scope::new(["personal"]);
        let inputs = |value| {
            vec![ScopedInput {
                value,
                scope: scope.clone(),
            }]
        };
        let workshop = Workshop::open(&path).unwrap();
        workshop
            .protect_cases(
                "titles",
                &[
                    EvaluationCase {
                        inputs: inputs(json!({"title":"First"})),
                        expected: json!(["First"]),
                    },
                    EvaluationCase {
                        inputs: inputs(json!({"other":"missing"})),
                        expected: json!([null]),
                    },
                ],
            )
            .unwrap();
        // Simulates a parsed provider-authored expression, not a prebuilt tool.
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
            authoring_evidence: "synthetic provider result for fixture".into(),
        };
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER fail_comparison BEFORE INSERT ON tool_comparisons BEGIN SELECT RAISE(FAIL,'simulated comparison receipt failure'); END;").unwrap();
        assert!(
            workshop
                .submit_candidate("titles", &candidate, None)
                .is_err()
        );
        let hash = crate::assistant_evolution::validate_definition(&candidate.definition).unwrap();
        assert!(workshop.approve_exact(&hash, scope.clone()).is_err());
        db.execute_batch("DROP TRIGGER fail_comparison").unwrap();
        let report = workshop
            .submit_candidate("titles", &candidate, None)
            .unwrap();
        assert!(report.passed);
        assert!(
            workshop
                .invoke("titles", &inputs(json!({"title":"Fresh"})), None)
                .is_err()
        );
        let _grant = workshop
            .approve_exact(&report.tool_hash, scope.clone())
            .unwrap();
        drop(workshop);
        let workshop = Workshop::open(&path).unwrap();
        assert_eq!(
            workshop
                .invoke("titles", &inputs(json!({"title":"Fresh"})), None)
                .unwrap()
                .value,
            json!(["Fresh"])
        );
        let mut second = candidate.clone();
        second.definition.version = 2;
        let report2 = workshop.submit_candidate("titles", &second, None).unwrap();
        assert!(report2.passed);
        workshop
            .approve_exact(&report2.tool_hash, scope.clone())
            .unwrap();
        assert_eq!(
            workshop
                .invoke("titles", &inputs(json!({"title":"New"})), None)
                .unwrap()
                .tool_hash,
            report2.tool_hash
        );
        let grant = workshop
            .rollback_exact("titles", &report.tool_hash, scope.clone())
            .unwrap();
        assert_eq!(
            workshop
                .invoke("titles", &inputs(json!({"title":"Rollback"})), None)
                .unwrap()
                .tool_hash,
            report.tool_hash
        );
        let mut bad = candidate;
        bad.definition.version = 3;
        bad.definition.expression = Expr::Literal {
            value: json!(["First"]),
        };
        let report = workshop.submit_candidate("titles", &bad, None).unwrap();
        assert!(!report.passed);
        assert!(workshop.approve_exact(&report.tool_hash, scope).is_err());
        workshop.revoke(&grant).unwrap();
        assert!(workshop.invoke("titles", &[], None).is_err());
    }

    #[test]
    fn active_baseline_regression_is_durable_and_does_not_replace_old_tool() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private/workshop.sqlite");
        let scope = Scope::new(["personal"]);
        let inputs = vec![ScopedInput {
            value: json!({"title": "First"}),
            scope: scope.clone(),
        }];
        let workshop = Workshop::open(&path).unwrap();
        workshop
            .protect_cases(
                "titles",
                &[EvaluationCase {
                    inputs: inputs.clone(),
                    expected: json!(["First"]),
                }],
            )
            .unwrap();
        let first = CandidateManifest {
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
            authoring_evidence: "v1".into(),
        };
        let first_report = workshop.submit_candidate("titles", &first, None).unwrap();
        workshop
            .approve_exact(&first_report.tool_hash, scope.clone())
            .unwrap();
        assert_eq!(
            workshop
                .latest_comparison("titles", &first_report.tool_hash)
                .unwrap()
                .unwrap()
                .verdict,
            "first_candidate"
        );
        let mut second = first.clone();
        second.definition.version = 2;
        second.definition.expression = Expr::Literal {
            value: json!(["Wrong"]),
        };
        let second_report = workshop.submit_candidate("titles", &second, None).unwrap();
        assert!(!second_report.passed);
        let comparison = workshop
            .latest_comparison("titles", &second_report.tool_hash)
            .unwrap()
            .unwrap();
        assert_eq!(
            comparison.baseline_hash.as_deref(),
            Some(first_report.tool_hash.as_str())
        );
        assert_eq!(comparison.candidate_passed, 0);
        assert_eq!(comparison.baseline_passed, Some(1));
        assert_eq!(comparison.verdict, "regression");
        assert_eq!(
            workshop.invoke("titles", &inputs, None).unwrap().tool_hash,
            first_report.tool_hash
        );
        drop(workshop);
        let reopened = Workshop::open(&path).unwrap();
        assert_eq!(
            reopened
                .latest_comparison("titles", &second_report.tool_hash)
                .unwrap()
                .unwrap()
                .verdict,
            "regression"
        );
        assert_eq!(
            reopened.invoke("titles", &inputs, None).unwrap().tool_hash,
            first_report.tool_hash
        );
    }

    #[test]
    fn candidate_inherits_baseline_suites_before_approval() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private/workshop.sqlite");
        let scope = Scope::new(["personal"]);
        let input = |value| {
            vec![ScopedInput {
                value,
                scope: scope.clone(),
            }]
        };
        let workshop = Workshop::open(&path).unwrap();
        workshop
            .protect_cases(
                "base",
                &[EvaluationCase {
                    inputs: input(json!({"v": 1})),
                    expected: json!([1]),
                }],
            )
            .unwrap();
        workshop
            .protect_cases(
                "easy",
                &[EvaluationCase {
                    inputs: input(json!({"v": 2})),
                    expected: json!(2),
                }],
            )
            .unwrap();
        let first = CandidateManifest {
            definition: ToolDefinition {
                name: "values".into(),
                version: 1,
                input_scope: scope.clone(),
                expression: Expr::Map {
                    input: Box::new(Expr::Input),
                    expr: Box::new(Expr::CurrentField { path: "v".into() }),
                },
            },
            authoring_evidence: "v1".into(),
        };
        let first_report = workshop.submit_candidate("base", &first, None).unwrap();
        workshop
            .approve_exact(&first_report.tool_hash, scope.clone())
            .unwrap();
        let mut second = first.clone();
        second.definition.version = 2;
        second.definition.expression = Expr::Literal { value: json!(2) };
        let second_report = workshop.submit_candidate("easy", &second, None).unwrap();
        assert!(!second_report.passed);
        assert!(
            workshop
                .approve_exact(&second_report.tool_hash, scope.clone())
                .is_err()
        );
        let base_comparison = workshop
            .latest_comparison("base", &second_report.tool_hash)
            .unwrap()
            .unwrap();
        assert_eq!(base_comparison.baseline_passed, Some(1));
        assert_eq!(base_comparison.candidate_passed, 0);
        assert_eq!(base_comparison.verdict, "regression");
        assert_eq!(
            workshop
                .invoke("values", &input(json!({"v": 3})), None)
                .unwrap()
                .tool_hash,
            first_report.tool_hash
        );

        let other_scope = Scope::new(["other"]);
        workshop
            .protect_cases(
                "other",
                &[EvaluationCase {
                    inputs: vec![ScopedInput {
                        value: json!({"v": 4}),
                        scope: other_scope.clone(),
                    }],
                    expected: json!([4]),
                }],
            )
            .unwrap();
        let mut isolated = first;
        isolated.definition.version = 3;
        isolated.definition.input_scope = other_scope.clone();
        let isolated_report = workshop.submit_candidate("other", &isolated, None).unwrap();
        let isolated_comparison = workshop
            .latest_comparison("other", &isolated_report.tool_hash)
            .unwrap()
            .unwrap();
        assert_eq!(isolated_comparison.baseline_hash, None);
        assert_eq!(isolated_comparison.verdict, "first_candidate");
    }
}
