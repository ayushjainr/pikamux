//! Durable, pure-data self-created tools.
//!
//! This module deliberately has no filesystem, process, network, or provider
//! capability.  A tool is a JSON AST plus a scope.  Registration, evaluation,
//! activation, and invocation are separate durable operations.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

fn bounded_json(
    value: &(impl Serialize + ?Sized),
    limit: usize,
) -> Result<Vec<u8>, EvolutionError> {
    struct Writer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
            if input.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("serialized JSON exceeds byte limit"));
            }
            self.bytes.extend_from_slice(input);
            Ok(input.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| EvolutionError::Limit("serialized JSON exceeds byte limit".into()))?;
    Ok(writer.bytes)
}

pub const MAX_DEFINITION_BYTES: usize = 256 * 1024;
pub const MAX_AST_DEPTH: usize = 32;
pub const MAX_INPUT_RECORDS: usize = 10_000;
pub const MAX_FUEL: u64 = 1_000_000;
pub const MAX_CHARGED_MEMORY: usize = 32 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_EXECUTION: Duration = Duration::from_secs(2);
pub const MAX_VALUE_DEPTH: usize = 64;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Debug, thiserror::Error)]
pub enum EvolutionError {
    #[error("invalid tool definition: {0}")]
    InvalidDefinition(String),
    #[error("tool resource limit exceeded: {0}")]
    Limit(String),
    #[error("tool was not found: {0}")]
    NotFound(String),
    #[error("tool is not eligible for activation: {0}")]
    NotEligible(String),
    #[error("activation approval does not match the exact tool or scope")]
    ApprovalMismatch,
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Scope {
    pub values: BTreeSet<String>,
}

impl Scope {
    pub fn new(values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    pub fn union<'a>(scopes: impl IntoIterator<Item = &'a Scope>) -> Self {
        let mut values = BTreeSet::new();
        for scope in scopes {
            values.extend(scope.values.iter().cloned());
        }
        Self { values }
    }

    pub fn contains(&self, other: &Scope) -> bool {
        other.values.is_subset(&self.values)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Expr {
    Input,
    Current,
    Field {
        path: String,
    },
    CurrentField {
        path: String,
    },
    Literal {
        value: Value,
    },
    Map {
        input: Box<Expr>,
        expr: Box<Expr>,
    },
    Filter {
        input: Box<Expr>,
        predicate: Box<Expr>,
    },
    Compare {
        comparison: CompareOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    And {
        items: Vec<Expr>,
    },
    Or {
        items: Vec<Expr>,
    },
    Not {
        input: Box<Expr>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub version: u64,
    pub input_scope: Scope,
    pub expression: Expr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopedInput {
    pub value: Value,
    pub scope: Scope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateManifest {
    pub definition: ToolDefinition,
    pub authoring_evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationCase {
    pub inputs: Vec<ScopedInput>,
    pub expected: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub tool_hash: String,
    pub passed: bool,
    pub cases: usize,
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivationApproval {
    pub tool_hash: String,
    pub scope: Scope,
    pub approval_id: String,
}

/// A policy-issued, opaque grant.  The fields that authorize activation are
/// private so an author/evaluator cannot forge an approval by constructing a
/// similarly shaped JSON value.
#[derive(Clone, Debug)]
pub struct ApprovalGrant {
    grant_id: String,
    tool_hash: String,
    scope: Scope,
}
impl ApprovalGrant {
    pub fn id(&self) -> &str {
        &self.grant_id
    }
}

pub struct Author<'a> {
    registry: &'a Registry,
}
pub struct Evaluator<'a> {
    registry: &'a Registry,
}
pub struct Policy<'a> {
    registry: &'a Registry,
}

impl<'a> Author<'a> {
    pub fn register(&self, candidate: &CandidateManifest) -> Result<String, EvolutionError> {
        self.registry.register(candidate)
    }
}

impl<'a> Evaluator<'a> {
    pub fn protect_suite(
        &self,
        suite_id: &str,
        cases: &[EvaluationCase],
    ) -> Result<(), EvolutionError> {
        self.registry.protect_suite(suite_id, cases)
    }

    pub fn evaluate_suite(
        &self,
        hash: &str,
        suite_id: &str,
        cancel: Option<&AtomicBool>,
    ) -> Result<EvaluationReport, EvolutionError> {
        self.registry.evaluate_suite(hash, suite_id, cancel)
    }

    pub(crate) fn designate_required(
        &self,
        hash: &str,
        suites: &[String],
    ) -> Result<(), EvolutionError> {
        self.registry.designate_required(hash, suites)
    }
}

impl<'a> Policy<'a> {
    pub(crate) fn issue_grant(
        &self,
        hash: &str,
        scope: Scope,
        expires_at: Option<f64>,
    ) -> Result<ApprovalGrant, EvolutionError> {
        self.registry.issue_grant(hash, scope, expires_at)
    }

    pub(crate) fn revoke(&self, grant_id: &str) -> Result<(), EvolutionError> {
        self.registry.revoke(grant_id)
    }

    pub(crate) fn activate(&self, grant: &ApprovalGrant) -> Result<(), EvolutionError> {
        self.registry.activate_grant(grant)
    }

    pub(crate) fn rollback(&self, name: &str, grant: &ApprovalGrant) -> Result<(), EvolutionError> {
        self.registry.rollback_grant(name, grant)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub value: Value,
    pub output_scope: Scope,
    pub tool_hash: String,
    /// Bounded interpreter accounting for a single evaluation.  These are
    /// observations only; callers must not treat them as a promise of future
    /// cost savings.
    pub fuel_used: u64,
    pub operations: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_warning: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComparisonReceipt {
    pub candidate_hash: String,
    pub baseline_hash: Option<String>,
    pub suite_id: String,
    pub candidate_passed: usize,
    pub baseline_passed: Option<usize>,
    pub candidate_fuel: u64,
    pub baseline_fuel: Option<u64>,
    pub candidate_operations: u64,
    pub baseline_operations: Option<u64>,
    pub verdict: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub hash: String,
    pub definition: ToolDefinition,
    pub authoring_evidence: String,
}

/// Validate structure and bounds without executing any user-authored code.
pub fn validate_definition(definition: &ToolDefinition) -> Result<String, EvolutionError> {
    if definition.name.trim().is_empty() || definition.name.len() > 256 {
        return Err(EvolutionError::InvalidDefinition("invalid name".into()));
    }
    validate_expr(&definition.expression, 0)?;
    let encoded = bounded_json(definition, MAX_DEFINITION_BYTES)?;
    if encoded.len() > MAX_DEFINITION_BYTES {
        return Err(EvolutionError::Limit("definition exceeds 256 KiB".into()));
    }
    let depth = ast_depth(&definition.expression)?;
    if depth > MAX_AST_DEPTH {
        return Err(EvolutionError::Limit("AST depth exceeds 32".into()));
    }
    let mut hash = Sha256::new();
    hash.update(&encoded);
    Ok(format!("{:x}", hash.finalize()))
}

fn ast_depth(expr: &Expr) -> Result<usize, EvolutionError> {
    let children: Vec<&Expr> = match expr {
        Expr::Input
        | Expr::Current
        | Expr::Field { .. }
        | Expr::CurrentField { .. }
        | Expr::Literal { .. } => vec![],
        Expr::Map { input, expr } => vec![input, expr],
        Expr::Filter { input, predicate } => vec![input, predicate],
        Expr::Compare { left, right, .. } => vec![left, right],
        Expr::And { items } | Expr::Or { items } => items.iter().collect(),
        Expr::Not { input } => vec![input],
    };
    let child_max = children
        .into_iter()
        .map(ast_depth)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    child_max
        .checked_add(1)
        .ok_or_else(|| EvolutionError::Limit("AST depth overflow".into()))
}

fn validate_expr(expr: &Expr, depth: usize) -> Result<(), EvolutionError> {
    if depth >= MAX_AST_DEPTH {
        return Err(EvolutionError::Limit("AST depth exceeds 32".into()));
    }
    match expr {
        Expr::Input | Expr::Current => {}
        Expr::Field { path } | Expr::CurrentField { path } => {
            if path.len() > MAX_DEFINITION_BYTES {
                return Err(EvolutionError::Limit("field path is too large".into()));
            }
        }
        Expr::Literal { value } => {
            measure_value(value, depth, MAX_DEFINITION_BYTES)?;
        }
        Expr::Map { input, expr } => {
            validate_expr(input, depth + 1)?;
            validate_expr(expr, depth + 1)?;
        }
        Expr::Filter { input, predicate } => {
            validate_expr(input, depth + 1)?;
            validate_expr(predicate, depth + 1)?;
        }
        Expr::Compare { left, right, .. } => {
            validate_expr(left, depth + 1)?;
            validate_expr(right, depth + 1)?;
        }
        Expr::And { items } | Expr::Or { items } => {
            for item in items {
                validate_expr(item, depth + 1)?;
            }
        }
        Expr::Not { input } => validate_expr(input, depth + 1)?,
    }
    Ok(())
}

fn measure_value(value: &Value, depth: usize, limit: usize) -> Result<usize, EvolutionError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(EvolutionError::Limit("JSON value depth exceeded".into()));
    }
    let size = match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 32,
        Value::String(string) => string.len().checked_add(2).unwrap_or(limit + 1),
        Value::Array(values) => {
            let mut total = 2usize;
            for item in values {
                total = total.saturating_add(measure_value(item, depth + 1, limit)?);
                if total > limit {
                    break;
                }
            }
            total
        }
        Value::Object(values) => {
            let mut total = 2usize;
            for (key, item) in values {
                total = total
                    .saturating_add(key.len())
                    .saturating_add(measure_value(item, depth + 1, limit)?);
                if total > limit {
                    break;
                }
            }
            total
        }
    };
    if size > limit {
        return Err(EvolutionError::Limit(
            "JSON value exceeds bounded memory".into(),
        ));
    }
    Ok(size)
}

struct Budget<'a> {
    fuel: u64,
    memory: usize,
    operations: u64,
    started: Instant,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> Budget<'a> {
    fn check(&mut self, fuel: u64) -> Result<(), EvolutionError> {
        self.operations = self
            .operations
            .checked_add(1)
            .ok_or_else(|| EvolutionError::Limit("operation counter overflow".into()))?;
        self.fuel = self
            .fuel
            .checked_sub(fuel)
            .ok_or_else(|| EvolutionError::Limit("fuel exceeded".into()))?;
        if self.started.elapsed() > MAX_EXECUTION {
            return Err(EvolutionError::Limit("execution deadline exceeded".into()));
        }
        if self.cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(EvolutionError::Limit("execution cancelled".into()));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), EvolutionError> {
        self.memory = self
            .memory
            .checked_add(bytes)
            .ok_or_else(|| EvolutionError::Limit("memory overflow".into()))?;
        if self.memory > MAX_CHARGED_MEMORY {
            return Err(EvolutionError::Limit(
                "charged memory exceeds 32 MiB".into(),
            ));
        }
        Ok(())
    }

    fn clone_value(&mut self, value: &Value, depth: usize) -> Result<Value, EvolutionError> {
        self.check(1)?;
        let estimate = measure_value(value, depth, MAX_CHARGED_MEMORY)?;
        self.charge(estimate)?;
        match value {
            Value::Null => Ok(Value::Null),
            Value::Bool(value) => Ok(Value::Bool(*value)),
            Value::Number(value) => Ok(Value::Number(value.clone())),
            Value::String(value) => Ok(Value::String(value.clone())),
            Value::Array(values) => {
                self.charge(values.len().saturating_mul(std::mem::size_of::<Value>()))?;
                let mut result = Vec::with_capacity(values.len());
                for value in values {
                    result.push(self.clone_value(value, depth + 1)?);
                }
                Ok(Value::Array(result))
            }
            Value::Object(values) => {
                self.charge(
                    values
                        .len()
                        .saturating_mul(std::mem::size_of::<(String, Value)>()),
                )?;
                let mut result = serde_json::Map::new();
                for (key, value) in values {
                    self.charge(key.len())?;
                    result.insert(key.clone(), self.clone_value(value, depth + 1)?);
                }
                Ok(Value::Object(result))
            }
        }
    }
}

pub fn execute(
    definition: &ToolDefinition,
    inputs: &[ScopedInput],
    cancel: Option<&AtomicBool>,
) -> Result<ToolResult, EvolutionError> {
    let hash = validate_definition(definition)?;
    if inputs.len() > MAX_INPUT_RECORDS {
        return Err(EvolutionError::Limit("input records exceed 10,000".into()));
    }
    let mut budget = Budget {
        fuel: MAX_FUEL,
        memory: 0,
        operations: 0,
        started: Instant::now(),
        cancel,
    };
    budget.charge(inputs.len().saturating_mul(std::mem::size_of::<Value>()))?;
    let mut root_values = Vec::with_capacity(inputs.len());
    for input in inputs {
        for scope in &input.scope.values {
            budget.charge(scope.len())?;
        }
        root_values.push(budget.clone_value(&input.value, 0)?);
    }
    let root = Value::Array(root_values);
    let value = eval(&definition.expression, &root, None, &mut budget)?;
    let output_bytes = measure_value(&value, 0, MAX_OUTPUT_BYTES)?;
    if output_bytes > MAX_OUTPUT_BYTES {
        return Err(EvolutionError::Limit("output exceeds 1 MiB".into()));
    }
    budget.charge(output_bytes)?;
    // Escapes, separators and object keys can make encoded output larger than
    // the in-memory estimate. Bound the actual serializer before allocation.
    let _ = bounded_json(&value, MAX_OUTPUT_BYTES)?;
    let output_scope = Scope::union(inputs.iter().map(|input| &input.scope));
    Ok(ToolResult {
        value,
        output_scope,
        tool_hash: hash,
        fuel_used: MAX_FUEL - budget.fuel,
        operations: budget.operations,
        provenance_warning: None,
    })
}

fn eval(
    expr: &Expr,
    root: &Value,
    current: Option<&Value>,
    budget: &mut Budget<'_>,
) -> Result<Value, EvolutionError> {
    budget.check(1)?;
    let result = match expr {
        Expr::Input => budget.clone_value(root, 0)?,
        Expr::Current => budget.clone_value(
            current.ok_or_else(|| {
                EvolutionError::InvalidDefinition("current used outside map/filter".into())
            })?,
            0,
        )?,
        Expr::Field { path } => clone_lookup(lookup(root, path), budget)?,
        Expr::CurrentField { path } => clone_lookup(
            lookup(
                current.ok_or_else(|| {
                    EvolutionError::InvalidDefinition("current field outside map/filter".into())
                })?,
                path,
            ),
            budget,
        )?,
        Expr::Literal { value } => budget.clone_value(value, 0)?,
        Expr::Map { input, expr } => {
            let evaluated = eval(input, root, current, budget)?;
            let array = evaluated.as_array().ok_or_else(|| {
                EvolutionError::InvalidDefinition("map input must be an array".into())
            })?;
            budget.charge(array.len().saturating_mul(std::mem::size_of::<Value>()))?;
            let mut out = Vec::with_capacity(array.len());
            for value in array {
                out.push(eval(expr, root, Some(value), budget)?);
            }
            Value::Array(out)
        }
        Expr::Filter { input, predicate } => {
            let evaluated = eval(input, root, current, budget)?;
            let array = evaluated.as_array().ok_or_else(|| {
                EvolutionError::InvalidDefinition("filter input must be an array".into())
            })?;
            budget.charge(array.len().saturating_mul(std::mem::size_of::<Value>()))?;
            let mut out = Vec::new();
            for value in array {
                if eval(predicate, root, Some(value), budget)?.as_bool() == Some(true) {
                    out.push(budget.clone_value(value, 0)?);
                }
            }
            Value::Array(out)
        }
        Expr::Compare {
            comparison,
            left,
            right,
        } => compare(
            *comparison,
            &eval(left, root, current, budget)?,
            &eval(right, root, current, budget)?,
        )?,
        Expr::And { items } => {
            let mut value = true;
            for item in items {
                if !eval(item, root, current, budget)?
                    .as_bool()
                    .unwrap_or(false)
                {
                    value = false;
                    break;
                }
            }
            Value::Bool(value)
        }
        Expr::Or { items } => {
            let mut value = false;
            for item in items {
                if eval(item, root, current, budget)?
                    .as_bool()
                    .unwrap_or(false)
                {
                    value = true;
                    break;
                }
            }
            Value::Bool(value)
        }
        Expr::Not { input } => Value::Bool(
            !eval(input, root, current, budget)?
                .as_bool()
                .unwrap_or(false),
        ),
    };
    budget.charge(0)?;
    Ok(result)
}

fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.')
        .try_fold(value, |cursor, part| cursor.get(part))
}

fn clone_lookup(value: Option<&Value>, budget: &mut Budget<'_>) -> Result<Value, EvolutionError> {
    match value {
        Some(value) => budget.clone_value(value, 0),
        None => Ok(Value::Null),
    }
}

fn compare(op: CompareOp, left: &Value, right: &Value) -> Result<Value, EvolutionError> {
    if matches!(op, CompareOp::Eq | CompareOp::Ne) {
        return Ok(Value::Bool(if matches!(op, CompareOp::Eq) {
            left == right
        } else {
            left != right
        }));
    }
    let ordering = left
        .as_f64()
        .zip(right.as_f64())
        .map(|(a, b)| a.total_cmp(&b))
        .or_else(|| left.as_str().zip(right.as_str()).map(|(a, b)| a.cmp(b)))
        .ok_or_else(|| {
            EvolutionError::InvalidDefinition("compare requires two numbers or strings".into())
        })?;
    Ok(Value::Bool(match op {
        CompareOp::Lt => ordering.is_lt(),
        CompareOp::Lte => !ordering.is_gt(),
        CompareOp::Gt => ordering.is_gt(),
        CompareOp::Gte => !ordering.is_lt(),
        CompareOp::Eq | CompareOp::Ne => unreachable!("handled before ordering"),
    }))
}

fn run_comparison_cases(
    definition: &ToolDefinition,
    cases: &[EvaluationCase],
    cancel: Option<&AtomicBool>,
) -> Result<(usize, u64, u64), EvolutionError> {
    let mut passed = 0;
    let mut fuel: u64 = 0;
    let mut operations: u64 = 0;
    let started = Instant::now();
    for case in cases {
        if started.elapsed() > MAX_EXECUTION
            || cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(EvolutionError::Limit(
                "comparison cancelled or deadline exceeded".into(),
            ));
        }
        match execute(definition, &case.inputs, cancel) {
            Ok(result) => {
                if started.elapsed() > MAX_EXECUTION
                    || cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
                {
                    return Err(EvolutionError::Limit(
                        "comparison cancelled or deadline exceeded".into(),
                    ));
                }
                fuel = fuel.saturating_add(result.fuel_used);
                operations = operations.saturating_add(result.operations);
                if result.value == case.expected {
                    passed += 1;
                }
            }
            // A protected-case error is a failed comparison case.  The
            // evaluator's durable report remains the eligibility authority.
            Err(error) => {
                if started.elapsed() > MAX_EXECUTION
                    || cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
                {
                    return Err(error);
                }
            }
        }
    }
    Ok((passed, fuel, operations))
}

pub struct Registry {
    connection: Mutex<Connection>,
}

impl Registry {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, EvolutionError> {
        let path = path.as_ref();
        crate::assistant_storage::database(path).map_err(|error| {
            EvolutionError::InvalidDefinition(format!("private assistant database: {error}"))
        })?;
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS tool_candidates (hash TEXT PRIMARY KEY, name TEXT NOT NULL, definition_json BLOB NOT NULL, authoring_evidence TEXT NOT NULL, created_at REAL NOT NULL); CREATE TABLE IF NOT EXISTS protected_suites (suite_id TEXT PRIMARY KEY, cases_json BLOB NOT NULL, suite_hash TEXT NOT NULL UNIQUE); CREATE TABLE IF NOT EXISTS candidate_required_suites (hash TEXT NOT NULL REFERENCES tool_candidates(hash), suite_id TEXT NOT NULL REFERENCES protected_suites(suite_id), PRIMARY KEY(hash,suite_id)); CREATE TABLE IF NOT EXISTS tool_evaluations (evaluation_id INTEGER PRIMARY KEY AUTOINCREMENT, hash TEXT NOT NULL REFERENCES tool_candidates(hash), suite_id TEXT NOT NULL REFERENCES protected_suites(suite_id), report_json BLOB NOT NULL, passed INTEGER NOT NULL, created_at REAL NOT NULL); CREATE TABLE IF NOT EXISTS tool_grants (grant_id TEXT PRIMARY KEY, hash TEXT NOT NULL REFERENCES tool_candidates(hash), scope_json BLOB NOT NULL, expires_at REAL, revoked INTEGER NOT NULL DEFAULT 0, created_at REAL NOT NULL); CREATE TABLE IF NOT EXISTS tool_activations (name TEXT PRIMARY KEY, hash TEXT NOT NULL REFERENCES tool_candidates(hash), grant_id TEXT NOT NULL REFERENCES tool_grants(grant_id), active INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS tool_comparisons (comparison_id INTEGER PRIMARY KEY AUTOINCREMENT, candidate_hash TEXT NOT NULL REFERENCES tool_candidates(hash), baseline_hash TEXT, suite_id TEXT NOT NULL REFERENCES protected_suites(suite_id), scope_json BLOB NOT NULL, candidate_passed INTEGER NOT NULL, baseline_passed INTEGER, candidate_fuel INTEGER NOT NULL, baseline_fuel INTEGER, candidate_operations INTEGER NOT NULL, baseline_operations INTEGER, verdict TEXT NOT NULL, receipt_json BLOB NOT NULL, created_at REAL NOT NULL);")?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn author(&self) -> Author<'_> {
        Author { registry: self }
    }
    pub fn evaluator(&self) -> Evaluator<'_> {
        Evaluator { registry: self }
    }
    pub fn policy(&self) -> Policy<'_> {
        Policy { registry: self }
    }

    fn register(&self, candidate: &CandidateManifest) -> Result<String, EvolutionError> {
        if candidate.authoring_evidence.len() > MAX_DEFINITION_BYTES {
            return Err(EvolutionError::Limit(
                "authoring evidence exceeds 256 KiB".into(),
            ));
        }
        let hash = validate_definition(&candidate.definition)?;
        let json = serde_json::to_vec(&candidate.definition)?;
        let db = self.connection.lock().unwrap();
        db.execute("INSERT OR IGNORE INTO tool_candidates(hash,name,definition_json,authoring_evidence,created_at) VALUES(?,?,?,?,strftime('%s','now'))", params![hash, candidate.definition.name, json, candidate.authoring_evidence])?;
        Ok(hash)
    }

    fn protect_suite(
        &self,
        suite_id: &str,
        cases: &[EvaluationCase],
    ) -> Result<(), EvolutionError> {
        if suite_id.is_empty() || cases.is_empty() {
            return Err(EvolutionError::InvalidDefinition(
                "protected suite must be named and non-empty".into(),
            ));
        }
        if cases.len() > MAX_INPUT_RECORDS {
            return Err(EvolutionError::Limit(
                "protected suite has too many cases".into(),
            ));
        }
        for case in cases {
            if case.inputs.len() > MAX_INPUT_RECORDS {
                return Err(EvolutionError::Limit(
                    "protected case has too many records".into(),
                ));
            }
            for input in &case.inputs {
                measure_value(&input.value, 0, MAX_DEFINITION_BYTES)?;
                let scope_bytes: usize = input.scope.values.iter().map(String::len).sum();
                if scope_bytes > MAX_DEFINITION_BYTES {
                    return Err(EvolutionError::Limit("protected scope is too large".into()));
                }
                measure_value(&case.expected, 0, MAX_OUTPUT_BYTES)?;
            }
        }
        let encoded = bounded_json(cases, MAX_DEFINITION_BYTES)?;
        if encoded.len() > MAX_DEFINITION_BYTES {
            return Err(EvolutionError::Limit(
                "protected suite exceeds 256 KiB".into(),
            ));
        }
        let mut digest = Sha256::new();
        digest.update(&encoded);
        let suite_hash = format!("{:x}", digest.finalize());
        let db = self.connection.lock().unwrap();
        let existing: Option<String> = db
            .query_row(
                "SELECT suite_hash FROM protected_suites WHERE suite_id=?",
                params![suite_id],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some_and(|old| old != suite_hash) {
            return Err(EvolutionError::ApprovalMismatch);
        }
        db.execute(
            "INSERT OR IGNORE INTO protected_suites(suite_id,cases_json,suite_hash) VALUES(?,?,?)",
            params![suite_id, encoded, suite_hash],
        )?;
        Ok(())
    }

    fn designate_required(&self, hash: &str, suites: &[String]) -> Result<(), EvolutionError> {
        if suites.is_empty() {
            return Err(EvolutionError::NotEligible(
                "candidate has no protected evaluator suites".into(),
            ));
        }
        let db = self.connection.lock().unwrap();
        let candidate_exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tool_candidates WHERE hash=?)",
            params![hash],
            |row| row.get(0),
        )?;
        if !candidate_exists {
            return Err(EvolutionError::NotFound(hash.into()));
        }
        let existing: Vec<String> = {
            let mut statement = db.prepare(
                "SELECT suite_id FROM candidate_required_suites WHERE hash=? ORDER BY suite_id",
            )?;
            statement
                .query_map(params![hash], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        let mut wanted = suites.to_vec();
        wanted.sort();
        wanted.dedup();
        if !existing.is_empty() {
            return if existing == wanted {
                Ok(())
            } else {
                Err(EvolutionError::ApprovalMismatch)
            };
        }
        for suite in &wanted {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM protected_suites WHERE suite_id=?)",
                params![suite],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(EvolutionError::NotFound(suite.clone()));
            }
        }
        for suite in wanted {
            db.execute(
                "INSERT INTO candidate_required_suites(hash,suite_id) VALUES(?,?)",
                params![hash, suite],
            )?;
        }
        Ok(())
    }

    fn evaluate_suite(
        &self,
        hash: &str,
        suite_id: &str,
        cancel: Option<&AtomicBool>,
    ) -> Result<EvaluationReport, EvolutionError> {
        let candidate = self
            .candidate(hash)?
            .ok_or_else(|| EvolutionError::NotFound(hash.into()))?;
        let cases_json: Vec<u8> = self
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT cases_json FROM protected_suites WHERE suite_id=?",
                params![suite_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| EvolutionError::NotFound(suite_id.into()))?;
        let cases: Vec<EvaluationCase> = serde_json::from_slice(&cases_json)?;
        let mut failures = Vec::new();
        let started = Instant::now();
        for (index, case) in cases.iter().enumerate() {
            if started.elapsed() > MAX_EXECUTION
                || cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                failures.push(
                    "suite cancelled or deadline exceeded; remaining cases were not passed".into(),
                );
                break;
            }
            match execute(&candidate.definition, &case.inputs, cancel) {
                Ok(result) if result.value == case.expected => {}
                Ok(_) => failures.push(format!("case {index}: output mismatch")),
                Err(error) => failures.push(format!("case {index}: {error}")),
            }
        }
        let report = EvaluationReport {
            tool_hash: hash.into(),
            passed: failures.is_empty() && !cases.is_empty(),
            cases: cases.len(),
            failures,
        };
        let encoded = serde_json::to_vec(&report)?;
        self.connection.lock().unwrap().execute("INSERT INTO tool_evaluations(hash,suite_id,report_json,passed,created_at) VALUES(?,?,?,?,strftime('%s','now'))", params![hash, suite_id, encoded, report.passed])?;
        Ok(report)
    }

    fn comparison_cases(&self, suite_id: &str) -> Result<Vec<EvaluationCase>, EvolutionError> {
        let bytes: Vec<u8> = self
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT cases_json FROM protected_suites WHERE suite_id=?",
                params![suite_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| EvolutionError::NotFound(suite_id.into()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub(crate) fn compare_candidate(
        &self,
        suite_id: &str,
        candidate_hash: &str,
        cancel: Option<&AtomicBool>,
    ) -> Result<ComparisonReceipt, EvolutionError> {
        let candidate = self
            .candidate(candidate_hash)?
            .ok_or_else(|| EvolutionError::NotFound(candidate_hash.into()))?;
        let cases = self.comparison_cases(suite_id)?;
        let (candidate_passed, candidate_fuel, candidate_operations) =
            run_comparison_cases(&candidate.definition, &cases, cancel)?;

        // The active grant is the only baseline authority.  A same-name tool
        // with a different scope is not a valid comparison and is reported as
        // no baseline rather than being silently widened.
        let baseline_hash: Option<String> = {
            let db = self.connection.lock().unwrap();
            let mut statement = db.prepare(
                "SELECT a.hash,g.scope_json,g.expires_at,g.revoked FROM tool_activations a JOIN tool_grants g ON g.grant_id=a.grant_id WHERE a.name=? AND a.active=1",
            )?;
            let mut rows = statement.query(params![candidate.definition.name])?;
            let mut found = None;
            while let Some(row) = rows.next()? {
                let hash: String = row.get(0)?;
                let scope: Scope = serde_json::from_slice(&row.get::<_, Vec<u8>>(1)?)?;
                let expiry: Option<f64> = row.get(2)?;
                let revoked: i64 = row.get(3)?;
                if hash != candidate_hash
                    && scope == candidate.definition.input_scope
                    && revoked == 0
                    && expiry.is_none_or(|value| value > now())
                {
                    found = Some(hash);
                    break;
                }
            }
            found
        };
        let baseline = baseline_hash
            .as_deref()
            .map(|hash| self.candidate(hash))
            .transpose()?
            .flatten();
        let (baseline_passed, baseline_fuel, baseline_operations) = match baseline {
            Some(record) if record.definition.input_scope == candidate.definition.input_scope => {
                let (passed, fuel, operations) =
                    run_comparison_cases(&record.definition, &cases, cancel)?;
                (Some(passed), Some(fuel), Some(operations))
            }
            _ => (None, None, None),
        };
        let verdict = match baseline_passed {
            None => "first_candidate",
            Some(previous) if candidate_passed > previous => "improvement",
            Some(previous) if candidate_passed < previous => "regression",
            Some(_) => "equivalent",
        }
        .to_owned();
        let receipt = ComparisonReceipt {
            candidate_hash: candidate_hash.into(),
            baseline_hash,
            suite_id: suite_id.into(),
            candidate_passed,
            baseline_passed,
            candidate_fuel,
            baseline_fuel,
            candidate_operations,
            baseline_operations,
            verdict,
        };
        let encoded = serde_json::to_vec(&receipt)?;
        self.connection.lock().unwrap().execute(
            "INSERT INTO tool_comparisons(candidate_hash,baseline_hash,suite_id,scope_json,candidate_passed,baseline_passed,candidate_fuel,baseline_fuel,candidate_operations,baseline_operations,verdict,receipt_json,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,strftime('%s','now'))",
            params![
                &receipt.candidate_hash,
                &receipt.baseline_hash,
                &receipt.suite_id,
                serde_json::to_vec(&candidate.definition.input_scope)?,
                receipt.candidate_passed as i64,
                receipt.baseline_passed.map(|value| value as i64),
                receipt.candidate_fuel as i64,
                receipt.baseline_fuel.map(|value| value as i64),
                receipt.candidate_operations as i64,
                receipt.baseline_operations.map(|value| value as i64),
                &receipt.verdict,
                encoded,
            ],
        )?;
        Ok(receipt)
    }

    pub(crate) fn active_baseline_hash(
        &self,
        name: &str,
        scope: &Scope,
    ) -> Result<Option<String>, EvolutionError> {
        let db = self.connection.lock().unwrap();
        let mut statement = db.prepare(
            "SELECT a.hash,g.scope_json,g.expires_at,g.revoked FROM tool_activations a JOIN tool_grants g ON g.grant_id=a.grant_id WHERE a.name=? AND a.active=1",
        )?;
        let mut rows = statement.query(params![name])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let stored_scope: Scope = serde_json::from_slice(&row.get::<_, Vec<u8>>(1)?)?;
            let expiry: Option<f64> = row.get(2)?;
            let revoked: i64 = row.get(3)?;
            if stored_scope == *scope && revoked == 0 && expiry.is_none_or(|value| value > now()) {
                return Ok(Some(hash));
            }
        }
        Ok(None)
    }

    pub(crate) fn required_suites(&self, hash: &str) -> Result<Vec<String>, EvolutionError> {
        let db = self.connection.lock().unwrap();
        let mut statement = db.prepare(
            "SELECT suite_id FROM candidate_required_suites WHERE hash=? ORDER BY suite_id",
        )?;
        Ok(statement
            .query_map(params![hash], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?)
    }

    fn issue_grant(
        &self,
        hash: &str,
        scope: Scope,
        expires_at: Option<f64>,
    ) -> Result<ApprovalGrant, EvolutionError> {
        if expires_at.is_some_and(|value| !value.is_finite() || value <= now()) {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let candidate = self
            .candidate(hash)?
            .ok_or_else(|| EvolutionError::NotFound(hash.into()))?;
        if !scope.contains(&candidate.definition.input_scope) {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let grant_id = format!("grant-{}", Uuid::new_v4());
        let scope_json = serde_json::to_vec(&scope)?;
        self.connection.lock().unwrap().execute("INSERT INTO tool_grants(grant_id,hash,scope_json,expires_at,created_at) VALUES(?,?,?,?,strftime('%s','now'))", params![grant_id, hash, scope_json, expires_at])?;
        Ok(ApprovalGrant {
            grant_id,
            tool_hash: hash.into(),
            scope,
        })
    }

    fn revoke(&self, grant_id: &str) -> Result<(), EvolutionError> {
        if self.connection.lock().unwrap().execute(
            "UPDATE tool_grants SET revoked=1 WHERE grant_id=?",
            params![grant_id],
        )? == 0
        {
            return Err(EvolutionError::NotFound(grant_id.into()));
        }
        Ok(())
    }

    fn activate_grant(&self, grant: &ApprovalGrant) -> Result<(), EvolutionError> {
        let candidate = self
            .candidate(&grant.tool_hash)?
            .ok_or_else(|| EvolutionError::NotFound(grant.tool_hash.clone()))?;
        let row: Option<(String, Vec<u8>, Option<f64>, i64)> = self
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT hash,scope_json,expires_at,revoked FROM tool_grants WHERE grant_id=?",
                params![grant.grant_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((hash, scope_json, expires_at, revoked)) = row else {
            return Err(EvolutionError::ApprovalMismatch);
        };
        let stored_scope: Scope = serde_json::from_slice(&scope_json)?;
        if hash != grant.tool_hash
            || stored_scope != grant.scope
            || revoked != 0
            || expires_at.is_some_and(|value| value <= now())
            || !stored_scope.contains(&candidate.definition.input_scope)
        {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let eligibility: (i64, i64) = self.connection.lock().unwrap().query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN EXISTS(SELECT 1 FROM tool_evaluations e WHERE e.hash=r.hash AND e.suite_id=r.suite_id AND e.passed=1) AND NOT EXISTS(SELECT 1 FROM tool_evaluations f WHERE f.hash=r.hash AND f.suite_id=r.suite_id AND f.passed=0) THEN 1 ELSE 0 END),0) FROM candidate_required_suites r WHERE r.hash=?",
            params![hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if eligibility.0 == 0 || eligibility.0 != eligibility.1 {
            return Err(EvolutionError::NotEligible(
                "all mandatory protected evaluator suites must pass with no failure history".into(),
            ));
        }
        self.connection.lock().unwrap().execute("INSERT INTO tool_activations(name,hash,grant_id,active) VALUES(?,?,?,1) ON CONFLICT(name) DO UPDATE SET hash=excluded.hash,grant_id=excluded.grant_id,active=1", params![candidate.definition.name, hash, grant.grant_id])?;
        Ok(())
    }

    fn rollback_grant(&self, name: &str, approval: &ApprovalGrant) -> Result<(), EvolutionError> {
        let candidate = self
            .candidate(&approval.tool_hash)?
            .ok_or_else(|| EvolutionError::NotFound(approval.tool_hash.clone()))?;
        if candidate.definition.name != name {
            return Err(EvolutionError::ApprovalMismatch);
        }
        self.activate_grant(approval)
    }

    pub fn invoke(
        &self,
        name: &str,
        inputs: &[ScopedInput],
        cancel: Option<&AtomicBool>,
    ) -> Result<ToolResult, EvolutionError> {
        let (hash, grant_id): (String, String) = self
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT hash,grant_id FROM tool_activations WHERE name=? AND active=1",
                params![name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| EvolutionError::NotFound(name.into()))?;
        let candidate = self
            .candidate(&hash)?
            .ok_or_else(|| EvolutionError::NotFound(hash.clone()))?;
        let (scope_json, expires_at, revoked): (Vec<u8>, Option<f64>, i64) =
            self.connection.lock().unwrap().query_row(
                "SELECT scope_json,expires_at,revoked FROM tool_grants WHERE grant_id=?",
                params![grant_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let scope: Scope = serde_json::from_slice(&scope_json)?;
        let actual = Scope::union(inputs.iter().map(|input| &input.scope));
        if revoked != 0
            || expires_at.is_some_and(|value| value <= now())
            || !scope.contains(&actual)
            || !scope.contains(&candidate.definition.input_scope)
        {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let result = execute(&candidate.definition, inputs, cancel)?;
        if !scope.contains(&result.output_scope) {
            return Err(EvolutionError::ApprovalMismatch);
        }
        let still_valid: bool = self.connection.lock().unwrap().query_row("SELECT revoked=0 AND (expires_at IS NULL OR expires_at>?) FROM tool_grants WHERE grant_id=?", params![now(), grant_id], |row| row.get(0))?;
        if !still_valid {
            return Err(EvolutionError::ApprovalMismatch);
        }
        Ok(result)
    }

    pub(crate) fn is_active_hash(
        &self,
        hash: &str,
        requested_scope: &Scope,
    ) -> Result<bool, EvolutionError> {
        let db = self.connection.lock().unwrap();
        let mut statement = db.prepare("SELECT g.scope_json,g.expires_at,g.revoked FROM tool_activations a JOIN tool_grants g ON g.grant_id=a.grant_id WHERE a.hash=? AND a.active=1")?;
        let mut rows = statement.query(params![hash])?;
        while let Some(row) = rows.next()? {
            let scope: Scope = serde_json::from_slice(&row.get::<_, Vec<u8>>(0)?)?;
            let expires_at: Option<f64> = row.get(1)?;
            let revoked: i64 = row.get(2)?;
            if revoked == 0
                && expires_at.is_none_or(|expiry| expiry > now())
                && scope.contains(requested_scope)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn latest_evaluation(
        &self,
        suite_id: &str,
        candidate_hash: &str,
    ) -> Result<Option<EvaluationReport>, EvolutionError> {
        let db = self.connection.lock().unwrap();
        let mut suites = db
            .prepare(
                "SELECT suite_id FROM candidate_required_suites WHERE hash=? ORDER BY suite_id",
            )?
            .query_map(params![candidate_hash], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if suites.is_empty() {
            suites.push(suite_id.to_owned());
        }
        let mut aggregate: Option<EvaluationReport> = None;
        let mut missing = Vec::new();
        for suite in suites {
            let json: Option<Vec<u8>> = db
                .query_row(
                    "SELECT report_json FROM tool_evaluations WHERE suite_id=? AND hash=? ORDER BY evaluation_id DESC LIMIT 1",
                    params![suite, candidate_hash],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(bytes) = json else {
                missing.push(format!("{suite}: required evaluation not completed"));
                continue;
            };
            let report: EvaluationReport = serde_json::from_slice(&bytes)?;
            let current = aggregate.get_or_insert_with(|| EvaluationReport {
                tool_hash: report.tool_hash.clone(),
                passed: true,
                cases: 0,
                failures: Vec::new(),
            });
            current.cases += report.cases;
            current.passed &= report.passed;
            current.failures.extend(
                report
                    .failures
                    .into_iter()
                    .map(|failure| format!("{suite}: {failure}")),
            );
        }
        Ok(aggregate.map(|mut report| {
            report.failures.extend(missing);
            report.passed &= report.cases != 0 && report.failures.is_empty();
            report
        }))
    }

    pub(crate) fn latest_comparison(
        &self,
        suite_id: &str,
        candidate_hash: &str,
    ) -> Result<Option<ComparisonReceipt>, EvolutionError> {
        let db = self.connection.lock().unwrap();
        let json: Option<Vec<u8>> = db
            .query_row(
                "SELECT receipt_json FROM tool_comparisons WHERE suite_id=? AND candidate_hash=? ORDER BY comparison_id DESC LIMIT 1",
                params![suite_id, candidate_hash],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|bytes| Ok(serde_json::from_slice(&bytes)?))
            .transpose()
    }

    pub fn candidate(&self, hash: &str) -> Result<Option<CandidateRecord>, EvolutionError> {
        let db = self.connection.lock().unwrap();
        db.query_row(
            "SELECT definition_json,authoring_evidence FROM tool_candidates WHERE hash=?",
            params![hash],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(json, evidence)| {
            Ok(CandidateRecord {
                hash: hash.into(),
                definition: serde_json::from_slice(&json)?,
                authoring_evidence: evidence,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn input(value: Value, scope: &[&str]) -> ScopedInput {
        ScopedInput {
            value,
            scope: Scope::new(scope.iter().copied()),
        }
    }
    fn tool(expr: Expr) -> CandidateManifest {
        CandidateManifest {
            definition: ToolDefinition {
                name: "changed".into(),
                version: 1,
                input_scope: Scope::new(["project:a"]),
                expression: expr,
            },
            authoring_evidence: "fixture".into(),
        }
    }

    #[test]
    fn composable_filter_map_compare_and_scope_union() {
        let expr = Expr::Map {
            input: Box::new(Expr::Filter {
                input: Box::new(Expr::Input),
                predicate: Box::new(Expr::Compare {
                    comparison: CompareOp::Gt,
                    left: Box::new(Expr::CurrentField { path: "n".into() }),
                    right: Box::new(Expr::Literal { value: json!(1) }),
                }),
            }),
            expr: Box::new(Expr::CurrentField {
                path: "name".into(),
            }),
        };
        let result = execute(
            &tool(expr).definition,
            &[
                input(json!({"n":2,"name":"a"}), &["project:a"]),
                input(json!({"n":0,"name":"b"}), &["project:b"]),
            ],
            None,
        )
        .unwrap();
        assert_eq!(result.value, json!(["a"]));
        assert!(result.output_scope.values.contains("project:b"));
    }

    #[test]
    fn registry_requires_evaluator_and_exact_approval_and_survives_restart() {
        let dir = std::env::temp_dir().join(format!("pika-evolution-{}", Uuid::new_v4()));
        let path = dir.join("registry.sqlite");
        let registry = Registry::open(&path).unwrap();
        let hash = registry
            .author()
            .register(&tool(Expr::Map {
                input: Box::new(Expr::Input),
                expr: Box::new(Expr::CurrentField {
                    path: "name".into(),
                }),
            }))
            .unwrap();
        let case = EvaluationCase {
            inputs: vec![input(json!({"name":"x"}), &["project:a"])],
            expected: json!(["x"]),
        };
        registry
            .evaluator()
            .protect_suite("protected", &[case])
            .unwrap();
        registry
            .evaluator()
            .designate_required(&hash, &["protected".into()])
            .unwrap();
        assert!(
            registry
                .evaluator()
                .evaluate_suite(&hash, "protected", None)
                .unwrap()
                .passed
        );
        let grant = registry
            .policy()
            .issue_grant(&hash, Scope::new(["project:a"]), None)
            .unwrap();
        registry.policy().activate(&grant).unwrap();
        drop(registry);
        let registry = Registry::open(&path).unwrap();
        assert_eq!(
            registry
                .invoke(
                    "changed",
                    &[input(json!({"name":"y"}), &["project:a"])],
                    None
                )
                .unwrap()
                .value,
            json!(["y"])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_candidate_and_limits_are_rejected() {
        let bad = tool(Expr::Current);
        assert!(execute(&bad.definition, &[input(json!(1), &["project:a"])], None).is_err());
        let many = vec![input(json!(1), &["project:a"]); MAX_INPUT_RECORDS + 1];
        assert!(execute(&tool(Expr::Input).definition, &many, None).is_err());
    }

    #[test]
    fn protected_failures_are_append_only_and_suite_is_immutable() {
        let dir = std::env::temp_dir().join(format!("pika-evolution-history-{}", Uuid::new_v4()));
        let path = dir.join("registry.sqlite");
        let registry = Registry::open(&path).unwrap();
        let hash = registry.author().register(&tool(Expr::Current)).unwrap();
        let evaluator = registry.evaluator();
        evaluator
            .protect_suite(
                "stable",
                &[EvaluationCase {
                    inputs: vec![input(json!(1), &["project:a"])],
                    expected: json!(2),
                }],
            )
            .unwrap();
        evaluator
            .designate_required(&hash, &["stable".into()])
            .unwrap();
        assert!(
            !evaluator
                .evaluate_suite(&hash, "stable", None)
                .unwrap()
                .passed
        );
        assert!(
            evaluator
                .protect_suite(
                    "stable",
                    &[EvaluationCase {
                        inputs: vec![input(json!(1), &["project:a"])],
                        expected: json!(1)
                    }]
                )
                .is_err()
        );
        let count: i64 = registry
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tool_evaluations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forged_scope_and_revoked_grant_cannot_invoke() {
        let dir = std::env::temp_dir().join(format!("pika-evolution-scope-{}", Uuid::new_v4()));
        let path = dir.join("registry.sqlite");
        let registry = Registry::open(&path).unwrap();
        let hash = registry.author().register(&tool(Expr::Input)).unwrap();
        registry
            .evaluator()
            .protect_suite(
                "scope",
                &[EvaluationCase {
                    inputs: vec![input(json!(1), &["project:a"])],
                    expected: json!([1]),
                }],
            )
            .unwrap();
        registry
            .evaluator()
            .designate_required(&hash, &["scope".into()])
            .unwrap();
        registry
            .evaluator()
            .evaluate_suite(&hash, "scope", None)
            .unwrap();
        let grant = registry
            .policy()
            .issue_grant(&hash, Scope::new(["project:a"]), None)
            .unwrap();
        registry.policy().activate(&grant).unwrap();
        assert!(
            registry
                .invoke("changed", &[input(json!(1), &["project:b"])], None)
                .is_err()
        );
        registry.policy().revoke(&grant.grant_id).unwrap();
        assert!(
            registry
                .invoke("changed", &[input(json!(1), &["project:a"])], None)
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn easy_pass_cannot_hide_mandatory_contrast_failure() {
        let dir = std::env::temp_dir().join(format!("pika-evolution-contrast-{}", Uuid::new_v4()));
        let path = dir.join("registry.sqlite");
        let registry = Registry::open(&path).unwrap();
        let hash = registry.author().register(&tool(Expr::Input)).unwrap();
        let evaluator = registry.evaluator();
        evaluator
            .protect_suite(
                "easy",
                &[EvaluationCase {
                    inputs: vec![input(json!(1), &["project:a"])],
                    expected: json!([1]),
                }],
            )
            .unwrap();
        evaluator
            .protect_suite(
                "contrast",
                &[EvaluationCase {
                    inputs: vec![input(json!(1), &["project:a"])],
                    expected: json!([2]),
                }],
            )
            .unwrap();
        evaluator
            .designate_required(&hash, &["easy".into(), "contrast".into()])
            .unwrap();
        assert!(
            evaluator
                .evaluate_suite(&hash, "easy", None)
                .unwrap()
                .passed
        );
        assert!(
            !evaluator
                .evaluate_suite(&hash, "contrast", None)
                .unwrap()
                .passed
        );
        let grant = registry
            .policy()
            .issue_grant(&hash, Scope::new(["project:a"]), None)
            .unwrap();
        assert!(registry.policy().activate(&grant).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explosive_map_is_bounded_without_cloning_the_input_array() {
        let definition = tool(Expr::Map {
            input: Box::new(Expr::Input),
            expr: Box::new(Expr::Current),
        })
        .definition;
        let inputs =
            vec![input(json!({"payload": "x".repeat(4096)}), &["project:a"]); MAX_INPUT_RECORDS];
        assert!(execute(&definition, &inputs, None).is_err());
    }
    #[test]
    fn encoded_output_limit_includes_json_escape_expansion() {
        let definition = tool(Expr::Input).definition;
        let value = Value::String("\u{0000}".repeat(MAX_OUTPUT_BYTES / 3));
        let inputs = vec![input(value, &["project:a"])];
        assert!(matches!(
            execute(&definition, &inputs, None),
            Err(EvolutionError::Limit(_))
        ));
    }
}
