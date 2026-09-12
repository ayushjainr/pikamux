use pikamux::model::{ObservationKind, Provider, Status, StatusObservation};
use pikamux::resolve::{
    EvidenceState, NameCandidate, NameResolutionError, SelectionEvidence, resolve_name,
};
use pikamux::status::{ProjectionFallback, project_status};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const ATTENTION: &str = include_str!("contracts/v1/attention.json");
const IDENTITY: &str = include_str!("contracts/v1/identity.json");
const MUTANTS: &str = include_str!("contracts/v1/mutants.json");

#[derive(Deserialize)]
struct ContractFile<T> {
    schema_version: u64,
    family: String,
    reference: Reference,
    cases: Vec<T>,
}

#[derive(Deserialize)]
struct Reference {
    version: String,
    source_commit: String,
    tests: Vec<String>,
}

#[derive(Deserialize)]
struct AttentionCase {
    id: String,
    live: bool,
    home_state: String,
    #[serde(default)]
    fallback: FixtureFallback,
    observations: Vec<StatusObservation>,
    expected: Value,
}

#[derive(Deserialize)]
struct FixtureFallback {
    #[serde(default = "parked")]
    status: Status,
    #[serde(default)]
    unread: bool,
    #[serde(default)]
    attention_reason: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    observed_at: f64,
}

impl Default for FixtureFallback {
    fn default() -> Self {
        Self {
            status: Status::Parked,
            unread: false,
            attention_reason: None,
            error: None,
            observed_at: 0.0,
        }
    }
}

#[derive(Deserialize)]
struct IdentityCase {
    id: String,
    query: String,
    candidates: Vec<FixtureCandidate>,
    expected: Value,
}

#[derive(Deserialize)]
struct FixtureCandidate {
    label: String,
    provider: Provider,
    session_id: String,
    #[serde(default)]
    active_thread_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    live: bool,
    #[serde(default)]
    exact_home: bool,
    #[serde(default = "parked")]
    status: Status,
    #[serde(default = "local")]
    local: bool,
    evidence: FixtureEvidence,
    #[serde(default)]
    provider_updated_at: Option<f64>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FixtureEvidence {
    Available,
    Archived,
    Missing,
    Unknown,
}

#[derive(Deserialize)]
struct MutantFile {
    schema_version: u64,
    reference_source_commit: String,
    mutants: Vec<Mutant>,
}

#[derive(Deserialize)]
struct Mutant {
    id: String,
    family: String,
    case: String,
    path: String,
    replacement: Value,
}

fn parked() -> Status {
    Status::Parked
}

fn local() -> bool {
    true
}

fn parse_contract<T: for<'de> Deserialize<'de>>(raw: &str) -> ContractFile<T> {
    let parsed: ContractFile<T> = serde_json::from_str(raw).expect("valid contract fixture");
    assert_eq!(parsed.schema_version, 1, "unknown contract schema");
    assert_eq!(parsed.reference.version, "0.5.0a4");
    assert_eq!(
        parsed.reference.source_commit,
        "36de1b1eed1a182ed4e608d47f95d362f15d571a"
    );
    assert!(
        !parsed.reference.tests.is_empty(),
        "fixture lacks provenance"
    );
    parsed
}

fn compare_literal(case_id: &str, expected: &Value, actual: &Value) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "contract {case_id} differed\nexpected: {}\n  actual: {}",
            serde_json::to_string_pretty(expected).unwrap(),
            serde_json::to_string_pretty(actual).unwrap()
        ))
    }
}

fn attention_results() -> HashMap<String, (Value, Value)> {
    let contracts: ContractFile<AttentionCase> = parse_contract(ATTENTION);
    assert_eq!(contracts.family, "attention");
    unique_ids(contracts.cases.iter().map(|case| case.id.as_str()));
    contracts
        .cases
        .into_iter()
        .map(|case| {
            let actual = serde_json::to_value(project_status(
                &case.observations,
                case.live,
                &case.home_state,
                ProjectionFallback {
                    status: case.fallback.status,
                    unread: case.fallback.unread,
                    attention_reason: case.fallback.attention_reason.as_deref(),
                    error: case.fallback.error.as_deref(),
                    observed_at: case.fallback.observed_at,
                },
            ))
            .expect("projection serializes");
            (case.id, (case.expected, actual))
        })
        .collect()
}

fn identity_results() -> HashMap<String, (Value, Value)> {
    let contracts: ContractFile<IdentityCase> = parse_contract(IDENTITY);
    assert_eq!(contracts.family, "identity_name_resolution");
    unique_ids(contracts.cases.iter().map(|case| case.id.as_str()));
    contracts
        .cases
        .into_iter()
        .map(|case| {
            let candidates: Vec<NameCandidate> = case
                .candidates
                .iter()
                .map(|item| NameCandidate {
                    provider: item.provider,
                    session_id: item.session_id.clone(),
                    active_thread_id: item.active_thread_id.clone(),
                    name: item.name.clone(),
                    live: item.live,
                    exact_home: item.exact_home,
                    status: item.status,
                    local: item.local,
                    evidence: SelectionEvidence {
                        state: match item.evidence {
                            FixtureEvidence::Available => EvidenceState::Available,
                            FixtureEvidence::Archived => EvidenceState::Archived,
                            FixtureEvidence::Missing => EvidenceState::Missing,
                            FixtureEvidence::Unknown => EvidenceState::Unknown,
                        },
                        canonical_cwd: item.cwd.clone(),
                        provider_updated_at: item.provider_updated_at,
                    },
                })
                .collect();
            let actual = match resolve_name(&case.query, &candidates) {
                Ok(indexes) => json!({
                    "choices": indexes
                        .into_iter()
                        .map(|index| case.candidates[index].label.clone())
                        .collect::<Vec<_>>()
                }),
                Err(NameResolutionError::SavedUnavailable) => {
                    json!({"error":"saved_unavailable"})
                }
                Err(NameResolutionError::NotFound) => json!({"error":"not_found"}),
            };
            (case.id, (case.expected, actual))
        })
        .collect()
}

fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) {
    let mut seen = HashSet::new();
    for id in ids {
        assert!(seen.insert(id), "duplicate contract id: {id}");
    }
}

#[test]
fn fixture_attention_matches_literal_contracts() {
    for (id, (expected, actual)) in attention_results() {
        compare_literal(&id, &expected, &actual).unwrap();
    }
}

#[test]
fn fixture_identity_matches_literal_contracts() {
    for (id, (expected, actual)) in identity_results() {
        compare_literal(&id, &expected, &actual).unwrap();
    }
}

#[test]
fn fixture_negative_mutants_are_rejected() {
    let attention = attention_results();
    let identity = identity_results();
    let mutants: MutantFile = serde_json::from_str(MUTANTS).expect("valid mutant fixture");
    assert_eq!(mutants.schema_version, 1);
    assert_eq!(
        mutants.reference_source_commit,
        "36de1b1eed1a182ed4e608d47f95d362f15d571a"
    );
    unique_ids(mutants.mutants.iter().map(|mutant| mutant.id.as_str()));

    for mutant in mutants.mutants {
        let results = match mutant.family.as_str() {
            "attention" => &attention,
            "identity" => &identity,
            other => panic!("unknown mutant family {other}"),
        };
        let (expected, correct) = results
            .get(&mutant.case)
            .unwrap_or_else(|| panic!("mutant {} names unknown case {}", mutant.id, mutant.case));
        compare_literal(&mutant.case, expected, correct).unwrap();

        let mut bad = correct.clone();
        let target = bad
            .pointer_mut(&mutant.path)
            .unwrap_or_else(|| panic!("mutant {} has invalid path {}", mutant.id, mutant.path));
        *target = mutant.replacement.clone();
        assert!(
            compare_literal(&mutant.case, expected, &bad).is_err(),
            "negative mutant {} was accepted",
            mutant.id
        );
    }
}

#[test]
fn fixtures_are_host_independent_and_contain_no_live_routes() {
    for raw in [ATTENTION, IDENTITY, MUTANTS] {
        for forbidden in [
            "/Users/",
            "/home/",
            "ssh_target",
            "transcript_path",
            "tmux_pane",
        ] {
            assert!(
                !raw.contains(forbidden),
                "fixture contains host or live-state marker {forbidden}"
            );
        }
    }
}

#[test]
fn observation_kinds_and_statuses_are_literal_not_candidate_constants() {
    let raw: Value = serde_json::from_str(ATTENTION).unwrap();
    let cases = raw["cases"].as_array().unwrap();
    assert!(
        cases
            .iter()
            .any(|case| case["expected"]["status"] == "OPEN TWICE")
    );
    assert!(
        cases
            .iter()
            .any(|case| case["expected"]["status"] == "NEEDS YOU")
    );
    assert!(
        cases
            .iter()
            .any(|case| case["expected"]["status"] == "PARKED")
    );
    assert_eq!(
        serde_json::to_value(ObservationKind::Safety).unwrap(),
        Value::String("safety".into())
    );
}
