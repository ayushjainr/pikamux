//! Trusted host boundary around model-authored pure transformations.
//!
//! A provider can submit a candidate, not a test suite or an approval. The host
//! owns contrasting cases and exact-version human approval. Generated ASTs
//! receive only scoped JSON inputs, never a reference to this service.
use crate::assistant_evolution::{
    CandidateManifest, ComparisonReceipt, EvaluationCase, EvaluationReport, EvolutionError,
    Registry, Scope, ScopedInput, ToolResult,
};
use std::{path::Path, sync::atomic::AtomicBool};

pub struct Workshop {
    registry: Registry,
}

impl Workshop {
    pub fn open(path: &Path) -> Result<Self, EvolutionError> {
        Ok(Self {
            registry: Registry::open(path)?,
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
    /// The provider response parser has no path to this method.
    pub fn approve_exact(
        &self,
        displayed_hash: &str,
        approved_scope: Scope,
        expiry: f64,
    ) -> Result<String, EvolutionError> {
        self.require_comparisons(displayed_hash)?;
        let grant =
            self.registry
                .policy()
                .issue_grant(displayed_hash, approved_scope, Some(expiry))?;
        self.registry.policy().activate(&grant)?;
        Ok(grant.id().to_owned())
    }

    pub fn rollback_exact(
        &self,
        name: &str,
        displayed_hash: &str,
        scope: Scope,
        expiry: f64,
    ) -> Result<String, EvolutionError> {
        self.require_comparisons(displayed_hash)?;
        let grant = self
            .registry
            .policy()
            .issue_grant(displayed_hash, scope, Some(expiry))?;
        self.registry.policy().rollback(name, &grant)?;
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
        self.registry.invoke(name, inputs, cancel)
    }

    pub fn is_active_hash(&self, hash: &str, scope: Scope) -> Result<bool, EvolutionError> {
        self.registry.is_active_hash(hash, &scope)
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
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 3600.;
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
        assert!(
            workshop
                .approve_exact(&hash, scope.clone(), expiry)
                .is_err()
        );
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
            .approve_exact(&report.tool_hash, scope.clone(), expiry)
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
            .approve_exact(&report2.tool_hash, scope.clone(), expiry)
            .unwrap();
        assert_eq!(
            workshop
                .invoke("titles", &inputs(json!({"title":"New"})), None)
                .unwrap()
                .tool_hash,
            report2.tool_hash
        );
        let grant = workshop
            .rollback_exact("titles", &report.tool_hash, scope.clone(), expiry)
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
        assert!(
            workshop
                .approve_exact(&report.tool_hash, scope, expiry)
                .is_err()
        );
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
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 3600.;
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
            .approve_exact(&first_report.tool_hash, scope.clone(), expiry)
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
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 3600.;
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
            .approve_exact(&first_report.tool_hash, scope.clone(), expiry)
            .unwrap();
        let mut second = first.clone();
        second.definition.version = 2;
        second.definition.expression = Expr::Literal { value: json!(2) };
        let second_report = workshop.submit_candidate("easy", &second, None).unwrap();
        assert!(!second_report.passed);
        assert!(
            workshop
                .approve_exact(&second_report.tool_hash, scope.clone(), expiry)
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
