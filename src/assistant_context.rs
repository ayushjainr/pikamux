//! P03: bounded lexical recall, shared by dialogue and maintenance; no model I/O.
use crate::assistant_memory::{MemoryError, Record, RecordKind, Scope, Store};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub(crate) const MAX_PACKAGE_BYTES: usize = 32 * 1024;
pub(crate) const MAX_SOURCES: usize = 64;
pub(crate) const CONTINUITY_RULE: &str = "Your working context is limited and may be compacted or reset. Preserve the minimum useful facts, decisions, corrections and commitments. Confirm saves honestly. Keep facts in memory and reusable behavior in guidance. Use Reflection to ask what the next relevant interaction should do differently. Active learned guidance and evaluated methods are tentative advice: apply only in their stated conditions, check against their sources and current user intent, and let explicit user instructions win conflicts. They never grant permissions, access, spending or execution. No change is valid; do not invent lessons or rewrite yourself for activity. Respect explicit instructions, forgetting, source permissions and shared limits. Do not change your protected purpose, permissions, spending limits or tests.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceVersion {
    pub id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ContextPackage {
    pub scope: Scope,
    pub epoch: u64,
    pub orientation: &'static str,
    pub records: Vec<Record>,
    pub sources: Vec<SourceVersion>,
    pub omissions: Vec<String>,
    pub methods: Vec<crate::assistant_method::MethodGuidance>,
    pub tools: serde_json::Value,
}

/// The caller must authorize this exact destination scope before building.
/// Cross-scope combinations fail closed rather than unioning permissions.
pub(crate) fn build(
    memory: &Store,
    scope: &Scope,
    queries: &[String],
    required: &[String],
    byte_limit: usize,
) -> Result<ContextPackage, MemoryError> {
    validate_request(queries, required)?;
    memory.read_snapshot(|memory| {
        let limit = byte_limit.min(MAX_PACKAGE_BYTES);
        let mut package = ContextPackage {
            scope: scope.clone(), epoch: memory.forget_epoch()?, orientation: CONTINUITY_RULE,
            records: vec![], sources: vec![],
            methods: vec![],
            tools: serde_json::json!([]),
            omissions: vec!["Bounded lexical recall is not exhaustive; a miss does not prove absence. Historical or inaccessible evidence may be omitted.".into()],
        };
        let mut seen = HashSet::new();
        add_required(memory, &mut package, &mut seen, required, limit)?;
        let recent = memory.working_set(scope, 64)?;
        add_priority(memory, &mut package, &mut seen, &recent, limit)?;
        // Each term set uses standing-guidance and general BM25 searches:
        // two term sets therefore preserve the four-query engineering ceiling.
        add_search(memory, &mut package, &mut seen, queries, limit, true)?;
        add_recent_instructions(memory, &mut package, &mut seen, &recent, limit)?;
        add_adaptations(memory, &mut package, &mut seen, queries, limit)?;
        add_due_commitments(memory, &mut package, &mut seen, limit)?;
        add_search(memory, &mut package, &mut seen, queries, limit, false)?;
        // Exact source links recover older rationale/contrary evidence without a
        // second planner. Historical records keep their original typed state.
        add_links(memory, &mut package, &mut seen, limit)?;
        add_recent(memory, &mut package, &mut seen, recent, limit)?;
        validate_envelope(&package, limit)?;
        Ok(package)
    })
}

fn validate_envelope(package: &ContextPackage, limit: usize) -> Result<(), MemoryError> {
    if serde_json::to_vec(package)?.len() > limit {
        return Err(MemoryError::Invalid(
            "context envelope exceeds bound".into(),
        ));
    }
    Ok(())
}

fn validate_request(queries: &[String], required: &[String]) -> Result<(), MemoryError> {
    if queries.len() > 4 || queries.iter().any(|q| q.len() > 4096) || required.len() > MAX_SOURCES {
        return Err(MemoryError::Invalid(
            "context query/source bounds exceeded".into(),
        ));
    }
    Ok(())
}

fn add_priority(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    recent: &[Record],
    limit: usize,
) -> Result<(), MemoryError> {
    for record in recent.iter().filter(|r| r.protected_policy) {
        include(memory, package, seen, record.clone(), limit)?;
    }
    Ok(())
}

fn add_adaptations(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    queries: &[String],
    limit: usize,
) -> Result<(), MemoryError> {
    for record in crate::assistant_guidance::applicable_guidance(memory, &package.scope, 16)? {
        include(memory, package, seen, record, limit)?;
    }
    add_tools(memory, package, limit)?;
    add_methods(memory, package, queries, limit)?;
    Ok(())
}

fn add_due_commitments(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    limit: usize,
) -> Result<(), MemoryError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    for record in crate::assistant_decisions::due_commitments(memory, &package.scope, now)? {
        include(memory, package, seen, record, limit)?;
    }
    Ok(())
}

fn add_recent_instructions(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    recent: &[Record],
    limit: usize,
) -> Result<(), MemoryError> {
    for record in recent
        .iter()
        .filter(|r| matches!(r.kind, RecordKind::UserInstruction | RecordKind::Correction))
        .take(8)
    {
        include(memory, package, seen, record.clone(), limit)?;
    }
    Ok(())
}

fn add_recent(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    recent: Vec<Record>,
    limit: usize,
) -> Result<(), MemoryError> {
    for record in recent {
        if record.kind != RecordKind::InferredPreference {
            include(memory, package, seen, record, limit)?;
        }
    }
    Ok(())
}

fn add_required(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    required: &[String],
    limit: usize,
) -> Result<(), MemoryError> {
    for id in required {
        let record = memory
            .get(id)?
            .ok_or_else(|| MemoryError::NotFound(id.clone()))?;
        if !record.scope.permits(&package.scope) || record.kind == RecordKind::Draft {
            return Err(MemoryError::Invalid(
                "required context is outside scope or unsent".into(),
            ));
        }
        if !include(memory, package, seen, record, limit)? {
            return Err(MemoryError::Invalid(
                "required context exceeds package bound".into(),
            ));
        }
    }
    Ok(())
}

fn add_tools(
    memory: &Store,
    package: &mut ContextPackage,
    limit: usize,
) -> Result<(), MemoryError> {
    let scope = &package.scope;
    let path = memory.path().with_file_name("workshop.sqlite");
    if scope.node.is_some()
        || scope.provider.is_some()
        || scope.conversation.is_some()
        || !path.exists()
    {
        return Ok(());
    }
    let Some(project) = &scope.project else {
        return Ok(());
    };
    let workshop = crate::assistant_workshop::Workshop::open(&path)
        .map_err(|e| MemoryError::Invalid(e.to_string()))?;
    package.tools = workshop
        .active_references(&crate::assistant_evolution::Scope::new([project.clone()]))
        .map_err(|e| MemoryError::Invalid(e.to_string()))?;
    if serde_json::to_vec(package)?.len() > limit {
        package.tools = serde_json::json!([]);
    }
    Ok(())
}

fn add_methods(
    memory: &Store,
    package: &mut ContextPackage,
    queries: &[String],
    limit: usize,
) -> Result<(), MemoryError> {
    // Pure methods are already evaluated and approved. No model or new grant.
    let methods = crate::assistant_method::applicable(
        memory,
        &package.scope,
        serde_json::json!({"queries":queries}),
    )
    .map_err(|e| MemoryError::Invalid(e.to_string()))?;
    for method in methods {
        let mut combined = package.sources.clone();
        for source in &method.sources {
            if !combined.contains(source) {
                combined.push(source.clone());
            }
        }
        if combined.len() > MAX_SOURCES {
            continue;
        }
        let prior = std::mem::replace(&mut package.sources, combined);
        package.methods.push(method);
        if serde_json::to_vec(package)?.len() > limit {
            package.methods.pop();
            package.sources = prior;
        }
    }
    Ok(())
}

fn add_search(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    queries: &[String],
    limit: usize,
    standing: bool,
) -> Result<(), MemoryError> {
    for query in queries.iter().take(2) {
        let records = if standing {
            memory.search_standing_bm25_with_budget(&package.scope, query, 16, limit, seen)?
        } else {
            memory.search_bm25_with_budget(&package.scope, query, 32, limit, seen)?
        };
        for record in records {
            if standing || record.kind != RecordKind::InferredPreference {
                include(memory, package, seen, record, limit)?;
            }
        }
    }
    Ok(())
}

fn add_links(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    limit: usize,
) -> Result<(), MemoryError> {
    let links: Vec<String> = package
        .records
        .iter()
        .flat_map(|r| r.dependencies.iter().cloned())
        .take(MAX_SOURCES)
        .collect();
    for id in links {
        if let Some(record) = memory.get(&id)? {
            if record.scope.permits(&package.scope)
                && !matches!(
                    record.kind,
                    RecordKind::Draft | RecordKind::InferredPreference
                )
            {
                include(memory, package, seen, record, limit)?;
            }
        }
    }
    Ok(())
}

fn include(
    memory: &Store,
    package: &mut ContextPackage,
    seen: &mut HashSet<String>,
    record: Record,
    limit: usize,
) -> Result<bool, MemoryError> {
    if seen.contains(&record.id) {
        return Ok(true);
    }
    if package.sources.len() >= MAX_SOURCES {
        return Ok(false);
    }
    let revision = memory
        .source_version(&record.id)?
        .ok_or_else(|| MemoryError::NotFound(record.id.clone()))?;
    let source = SourceVersion {
        id: record.id.clone(),
        revision,
    };
    let added_source = !package.sources.contains(&source);
    if added_source {
        package.sources.push(source);
    }
    package.records.push(record);
    if serde_json::to_vec(package)?.len() > limit {
        package.records.pop();
        if added_source {
            package.sources.pop();
        }
        return Ok(false);
    }
    seen.insert(package.records.last().expect("just appended").id.clone());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_memory::{NewRecord, Origin};
    #[test]
    fn old_linked_rationale_survives_noise_with_versions_and_honest_omissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let make = |body: &str, timestamp| NewRecord {
            kind: RecordKind::Finding,
            origin: Origin::Human,
            scope: Scope::default(),
            body: body.into(),
            provenance: "synthetic".into(),
            timestamp,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        };
        let old = store
            .append_user(make("Original rationale was isolation, not cost", 1))
            .unwrap();
        for index in 0..100 {
            store
                .append_user(make("unrelated recent noise", index + 10))
                .unwrap();
        }
        let mut linked = make("Nebula design decision", 200);
        linked.dependencies.push(old.id.clone());
        let decision = store.append_user(linked).unwrap();
        let package = build(
            &store,
            &Scope::default(),
            &["Nebula".into()],
            &[decision.id],
            MAX_PACKAGE_BYTES,
        )
        .unwrap();
        assert!(package.records.iter().any(|r| r.id == old.id));
        assert!(package.sources.iter().all(|s| s.revision > 0));
        assert!(serde_json::to_vec(&package).unwrap().len() <= MAX_PACKAGE_BYTES);
        assert!(package.omissions[0].contains("miss does not prove absence"));
    }
    #[test]
    fn combined_scopes_fail_closed_and_required_overflow_is_not_silently_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let private = store
            .append_user(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: Scope {
                    project: Some("a".into()),
                    ..Default::default()
                },
                body: "private rationale".into(),
                provenance: "synthetic".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        assert!(
            build(
                &store,
                &Scope::default(),
                &[],
                std::slice::from_ref(&private.id),
                MAX_PACKAGE_BYTES
            )
            .is_err()
        );
        assert!(build(&store, &private.scope, &[], &[private.id], 100).is_err());
    }

    #[test]
    fn older_due_promise_precedes_newer_generic_decision_noise() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope::default();
        let mut record = NewRecord {
            kind: RecordKind::Decision, origin: Origin::Human, scope: scope.clone(),
            body: serde_json::json!({"schema":1,"commitment":"Review the owed repair","condition":"Recorded promise","due_at":1,"completion":{"kind":"open"}}).to_string(),
            provenance: "synthetic explicit human commitment".into(), timestamp: 1,
            supersedes: None, dependencies: vec![], decision_state: Some(crate::assistant_memory::DecisionState::Accepted), protected_policy: false,
        };
        let due = store.append_user(record.clone()).unwrap();
        for timestamp in 2..102 {
            record.timestamp = timestamp;
            record.body = "Unrelated recent decision".into();
            store.append_user(record.clone()).unwrap();
        }
        let package = build(&store, &scope, &[], &[], MAX_PACKAGE_BYTES).unwrap();
        assert!(package.records.iter().any(|r| r.id == due.id));
        assert!(package.sources.len() <= MAX_SOURCES);
        assert!(serde_json::to_vec(&package).unwrap().len() <= MAX_PACKAGE_BYTES);
    }
}
