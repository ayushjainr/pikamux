//! Explicit human controls for the existing pure workshop's method lane.
use crate::assistant_evolution::{CandidateManifest, EvaluationCase};
use crate::assistant_memory::{Scope, Store};
use crate::assistant_method::{MethodCases, MethodWorkshop};
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestRequest {
    proposal_id: String,
    experiment: String,
    candidate: CandidateManifest,
    original_failure: Vec<EvaluationCase>,
    contrasting: Vec<EvaluationCase>,
    protected: Vec<EvaluationCase>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assessment {
    hash: String,
    outcome: String,
    evidence: String,
    rollback: Option<String>,
}

pub(crate) fn handle_cancellable(
    memory: &mut Store,
    scope: &Scope,
    operation: &str,
    input: &str,
    _now: i64,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        bail!("Native method operation cancelled");
    }
    if input.len() > 32 * 1024 {
        bail!("Method control input exceeds 32 KiB");
    }
    if operation == "pending" {
        return Ok(
            json!({"proposals":crate::assistant_workshop_handoff::pending(memory,scope,32)?,"notice":"Saved proposals are not activated methods or tools."}),
        );
    }
    let workshop = MethodWorkshop::open(&memory.path().with_file_name("workshop.sqlite"))?;
    match operation {
        "test" => {
            let request: TestRequest = serde_json::from_str(input)?;
            test_candidate(memory, scope, &workshop, request, cancel)
        }
        "approve" => Ok(
            json!({"grant":workshop.approve_exact(memory,scope,input.trim())?,"notice":"This exact evaluated method is enabled for this scope without time expiry. Revocation, retirement and source validity still apply."}),
        ),
        "assess" => {
            let request: Assessment = serde_json::from_str(input)?;
            Ok(workshop.assess(
                scope,
                &request.hash,
                &request.outcome,
                &request.evidence,
                request.rollback.as_deref(),
            )?)
        }
        _ => bail!("Unknown native method control"),
    }
}

fn test_candidate(
    memory: &mut Store,
    scope: &Scope,
    workshop: &MethodWorkshop,
    request: TestRequest,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    let (source_scope, proposal) =
        crate::assistant_workshop_handoff::proposal(memory, &request.proposal_id)?;
    if source_scope != *scope {
        bail!("Method proposal must match this exact scope");
    }
    if matches!(
        proposal.kind,
        crate::assistant_workshop_handoff::ProposalKind::Tool
    ) {
        return test_tool(memory, request, cancel);
    }
    workshop.protect_cases(
        memory,
        &request.experiment,
        MethodCases {
            original_failure: request.original_failure,
            contrasting: request.contrasting,
            protected: request.protected,
        },
    )?;
    let report = workshop.submit(
        memory,
        &request.proposal_id,
        &request.experiment,
        &request.candidate,
        cancel,
    )?;
    Ok(
        json!({"evaluation":report,"notice":"Pure native evaluation only. Inspect the exact definition with /tool-info HASH; approval is separate."}),
    )
}

fn test_tool(
    memory: &mut Store,
    request: TestRequest,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    if request.original_failure.is_empty()
        || request.contrasting.is_empty()
        || request.protected.is_empty()
    {
        bail!("All three protected case roles are required");
    }
    let cases: Vec<_> = request
        .original_failure
        .into_iter()
        .chain(request.contrasting)
        .chain(request.protected)
        .collect();
    if cases.len() > 16 {
        bail!("At most 16 protected cases");
    }
    let mut distinct = std::collections::BTreeSet::new();
    for case in &cases {
        if !distinct.insert(serde_json::to_string(&case.inputs)?) {
            bail!("Original, contrasting and protected cases must exercise distinct inputs");
        }
    }
    let native = crate::assistant_workshop::Workshop::open(
        &memory.path().with_file_name("workshop.sqlite"),
    )?;
    native.protect_cases(&request.experiment, &cases)?;
    Ok(
        json!({"evaluation":native.submit_handoff_candidate(memory, &request.proposal_id, &request.experiment, &request.candidate, cancel)?,"notice":"Tool evaluated, not activated. Inspect /tool-info HASH before exact /approve HASH."}),
    )
}
