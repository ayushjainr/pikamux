//! Strict, source-backed subset of Claude 2.1.286 Bmr parallel-response repair.
//! Collapse proven response batches, require a unique quotient ancestry, then
//! expand in durable source order subject to every original internal parent edge.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

type Records = BTreeMap<String, Value>;
type Children = BTreeMap<Option<String>, String>;
pub(super) type ResultSources = BTreeMap<String, (Option<String>, bool)>;
const AMBIGUOUS: &str =
    "Claude history contains a rewind or branching ambiguity; read it in the original terminal";

fn response_key(id: &str, record: &Value) -> (String, String, String) {
    let response = record["message"]["id"]
        .as_str()
        .map(|id| format!("message:{id}"))
        .unwrap_or_else(|| format!("uuid:{id}"));
    (
        response,
        record["agentId"].to_string(),
        record["isSidechain"].to_string(),
    )
}

pub(super) fn result_sources(records: &Records) -> ResultSources {
    let mut calls = BTreeMap::<_, BTreeSet<&str>>::new();
    for (id, record) in records {
        if record["type"] == "assistant" && record["message"]["role"] == "assistant" {
            calls
                .entry(response_key(id, record))
                .or_default()
                .extend(blocks(record, "tool_use", "id"));
        }
    }
    records
        .iter()
        .filter(|(_, record)| {
            record["type"] == "user" && !blocks(record, "tool_result", "tool_use_id").is_empty()
        })
        .map(|(id, record)| {
            let parent = record["parentUuid"].as_str().map(str::to_owned);
            let verified = parent
                .as_deref()
                .and_then(|id| records.get_key_value(id))
                .is_some_and(|(parent_id, parent)| {
                    source_result_verified(record, parent_id, parent, &calls)
                });
            (id.clone(), (parent, verified))
        })
        .collect()
}

fn source_result_verified(
    record: &Value,
    parent_id: &str,
    parent: &Value,
    calls: &BTreeMap<(String, String, String), BTreeSet<&str>>,
) -> bool {
    parent["type"] == "assistant"
        && parent["message"]["role"] == "assistant"
        && record["message"]["role"] == "user"
        && record["agentId"] == parent["agentId"]
        && record["isSidechain"] == parent["isSidechain"]
        && calls
            .get(&response_key(parent_id, parent))
            .is_some_and(|calls| {
                blocks(record, "tool_result", "tool_use_id")
                    .iter()
                    .all(|id| calls.contains(id))
            })
}

pub(super) fn chain(
    mut records: Records,
    order: &[String],
    sources: &ResultSources,
    selector: Result<Option<String>>,
) -> Result<Vec<Value>> {
    let membership = membership(&records, sources)?;
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    for id in order.iter().filter(|id| records.contains_key(*id)) {
        groups
            .entry(membership[id].clone())
            .or_default()
            .push(id.clone());
    }
    let (children, selected_by_provider) = select_quotient(&records, &membership, selector)?;
    if selected_by_provider {
        let selected: BTreeSet<_> = children.values().collect();
        groups.retain(|id, _| selected.contains(id));
    }
    let mut current = children.get(&None).cloned();
    let mut output = Vec::new();
    while let Some(group) = current {
        let ids = groups
            .remove(&group)
            .context("Claude native ancestry contains a cycle")?;
        for id in expand(&ids, &records, &membership)? {
            output.push(
                records
                    .remove(&id)
                    .context("Claude batch identity missing")?,
            );
        }
        current = children.get(&Some(group)).cloned();
    }
    ensure!(
        groups.is_empty(),
        "Claude native ancestry is disconnected or cyclic; read it in the original terminal"
    );
    validate_causal_order(&output)?;
    Ok(output)
}

fn select_quotient(
    records: &Records,
    members: &BTreeMap<String, String>,
    selector: Result<Option<String>>,
) -> Result<(Children, bool)> {
    match quotient(records, members, None) {
        Ok(children) => Ok((children, false)),
        Err(ambiguity) => match selector? {
            Some(selector) => Ok((quotient(records, members, Some(&selector))?, true)),
            None => Err(ambiguity),
        },
    }
}

fn validate_causal_order(records: &[Value]) -> Result<()> {
    let retained_calls: BTreeSet<_> = records
        .iter()
        .filter(|record| record["type"] == "assistant")
        .flat_map(|record| blocks(record, "tool_use", "id"))
        .collect();
    let mut seen = BTreeSet::new();
    for record in records {
        if record["type"] == "assistant" {
            seen.extend(blocks(record, "tool_use", "id"));
        } else if record["type"] == "user" {
            // Native source order is authoritative. Do not move a result after
            // its call or let sequential receipt projection silently lose it.
            // Compacted-away calls have separate original-source proof; this
            // check only governs calls that remain in the projected ancestry.
            ensure!(
                blocks(record, "tool_result", "tool_use_id")
                    .iter()
                    .all(|id| !retained_calls.contains(id) || seen.contains(id)),
                "Claude tool result precedes its retained native call; read it in the original terminal"
            );
        }
    }
    Ok(())
}

fn blocks<'a>(record: &'a Value, kind: &str, field: &str) -> Vec<&'a str> {
    record["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == kind)
        .filter_map(|block| block[field].as_str())
        .collect()
}

fn membership(records: &Records, sources: &ResultSources) -> Result<BTreeMap<String, String>> {
    let mut members: BTreeMap<_, _> = records.keys().map(|id| (id.clone(), id.clone())).collect();
    let mut responses = BTreeMap::<&str, Vec<&str>>::new();
    for (id, record) in records {
        validate_blocks(record)?;
        if record["type"] == "assistant" {
            if let Some(response) = record["message"]["id"].as_str() {
                ensure!(!response.is_empty(), "Claude response identity missing");
                responses.entry(response).or_default().push(id);
            }
        }
    }
    for ids in responses.values() {
        assign_response(records, ids, &mut members)?;
    }
    assign_results(records, &mut members, sources)?;
    assign_bridges(records, &mut members)?;
    assign_tails(records, &mut members)?;
    Ok(members)
}

fn assign_tails(records: &Records, members: &mut BTreeMap<String, String>) -> Result<()> {
    let calls = call_index(records, members)?;
    let batches = proven_batches(records, members, &calls);
    let mut counts = BTreeMap::<&str, usize>::new();
    for record in records.values() {
        if let Some(parent) = record["parentUuid"].as_str() {
            *counts.entry(parent).or_default() += 1;
        }
    }
    for id in records
        .keys()
        .filter(|id| auxiliary(&records[*id]) && !counts.contains_key(id.as_str()))
    {
        let (path, anchor) = tail_path(id, records, &counts);
        let Some(anchor) = anchor else {
            continue;
        };
        let group = members[anchor].clone();
        if !batches.contains(&group) {
            continue;
        }
        for id in path {
            ensure!(
                records[id]["agentId"] == records[anchor]["agentId"]
                    && records[id]["isSidechain"] == records[anchor]["isSidechain"],
                "Claude auxiliary tail ownership conflicts"
            );
            members.insert(id.into(), group.clone());
        }
    }
    Ok(())
}

fn tail_path<'a>(
    id: &'a str,
    records: &'a Records,
    counts: &BTreeMap<&str, usize>,
) -> (Vec<&'a str>, Option<&'a str>) {
    let mut path = Vec::new();
    let mut current = Some(id);
    while let Some(id) = current {
        if !auxiliary(&records[id]) {
            return (path, Some(id));
        }
        if counts.get(id).copied().unwrap_or(0) > 1 {
            return (Vec::new(), None);
        }
        path.push(id);
        current = records[id]["parentUuid"].as_str();
    }
    (Vec::new(), None)
}

fn auxiliary(record: &Value) -> bool {
    match record["type"].as_str() {
        Some("attachment") => true,
        Some("system") => record["subtype"] != "compact_boundary",
        Some("user") => {
            record["isMeta"] == true && blocks(record, "tool_result", "tool_use_id").is_empty()
        }
        _ => false,
    }
}

fn auxiliary_anchors(records: &Records) -> Result<BTreeMap<String, Option<String>>> {
    let mut anchors = BTreeMap::<String, Option<String>>::new();
    for id in records.keys().filter(|id| auxiliary(&records[*id])) {
        let mut path = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = Some(id.as_str());
        let anchor = loop {
            let Some(id) = current else {
                break None;
            };
            if let Some(anchor) = anchors.get(id) {
                break anchor.clone();
            }
            if !auxiliary(&records[id]) {
                break Some(id.to_owned());
            }
            ensure!(
                seen.insert(id),
                "Claude auxiliary ancestry contains a cycle"
            );
            path.push(id.to_owned());
            current = records[id]["parentUuid"].as_str();
        };
        for id in path {
            anchors.insert(id, anchor.clone());
        }
    }
    Ok(anchors)
}

fn assign_bridges(records: &Records, members: &mut BTreeMap<String, String>) -> Result<()> {
    let anchors = auxiliary_anchors(records)?;
    for (id, record) in records
        .iter()
        .filter(|(_, record)| record["type"] == "assistant")
    {
        let Some(parent) = record["parentUuid"].as_str() else {
            continue;
        };
        let Some(Some(anchor)) = anchors.get(parent) else {
            continue;
        };
        let group = members[id].clone();
        if members[anchor] != group {
            continue;
        }
        let mut current = parent;
        while auxiliary(&records[current]) && members[current] != group {
            ensure!(
                records[current]["agentId"] == record["agentId"]
                    && records[current]["isSidechain"] == record["isSidechain"],
                "Claude auxiliary batch ownership conflicts"
            );
            members.insert(current.into(), group.clone());
            current = records[current]["parentUuid"]
                .as_str()
                .context("Claude auxiliary batch parent missing")?;
        }
    }
    Ok(())
}

fn validate_blocks(record: &Value) -> Result<()> {
    for block in record["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let field = match block["type"].as_str() {
            Some("tool_use") => "id",
            Some("tool_result") => "tool_use_id",
            _ => continue,
        };
        ensure!(
            block[field].as_str().is_some_and(|id| !id.is_empty()),
            "Claude tool block identity missing"
        );
    }
    Ok(())
}

fn assign_response(
    records: &Records,
    ids: &[&str],
    members: &mut BTreeMap<String, String>,
) -> Result<()> {
    if ids.len() < 2 {
        return Ok(());
    }
    let first = &records[ids[0]];
    if !ids
        .iter()
        .any(|id| !blocks(&records[*id], "tool_use", "id").is_empty())
    {
        return Ok(());
    }
    for id in ids {
        let record = &records[*id];
        ensure!(
            record["agentId"] == first["agentId"] && record["isSidechain"] == first["isSidechain"],
            "Claude response batch ownership conflicts"
        );
        ensure!(
            record["message"]["role"] == "assistant",
            "Claude response role conflicts"
        );
        members.insert((*id).into(), ids[0].into());
    }
    Ok(())
}

fn assign_results(
    records: &Records,
    members: &mut BTreeMap<String, String>,
    sources: &ResultSources,
) -> Result<()> {
    let calls = call_index(records, members)?;
    let batches = proven_batches(records, members, &calls);
    for (id, record) in records {
        let result_ids = blocks(record, "tool_result", "tool_use_id");
        if record["type"] != "user" || result_ids.is_empty() {
            continue;
        }
        let (source_parent, verified) = sources
            .get(id)
            .context("Claude original tool result identity missing")?;
        ensure!(
            *verified,
            "Claude tool result has no exact original call identity"
        );
        let Some(parent) = source_parent
            .as_deref()
            .and_then(|id| records.get_key_value(id))
        else {
            // Its original call was validated before native compaction removed
            // that assistant. Preserve the relinked node, without inventing a
            // replacement response group.
            continue;
        };
        ensure!(
            parent.1["type"] == "assistant",
            "Claude tool result requires native call ancestry reconstruction"
        );
        let group = members[parent.0].clone();
        ensure!(
            calls
                .get(&group)
                .is_some_and(|calls| result_ids.iter().all(|id| calls.contains(id))),
            "Claude tool result has no exact batch call identity"
        );
        ensure!(
            record["message"]["role"] == "user"
                && record["agentId"] == parent.1["agentId"]
                && record["isSidechain"] == parent.1["isSidechain"],
            "Claude tool result batch ownership conflicts"
        );
        if batches.contains(&group) {
            members.insert(id.clone(), group);
        }
    }
    Ok(())
}

fn proven_batches(
    records: &Records,
    members: &BTreeMap<String, String>,
    calls: &BTreeMap<String, BTreeSet<&str>>,
) -> BTreeSet<String> {
    let mut assistants = BTreeMap::<&str, usize>::new();
    let mut results = BTreeMap::<&str, usize>::new();
    for (id, record) in records {
        if record["type"] == "assistant" {
            *assistants.entry(&members[id]).or_default() += 1;
        } else if record["type"] == "user"
            && !blocks(record, "tool_result", "tool_use_id").is_empty()
        {
            if let Some(parent) = record["parentUuid"]
                .as_str()
                .filter(|parent| records[*parent]["type"] == "assistant")
            {
                *results.entry(&members[parent]).or_default() += 1;
            }
        }
    }
    assistants
        .into_iter()
        .filter(|(group, count)| {
            *count > 1
                || (results.get(group).copied().unwrap_or(0) > 1
                    && calls.get(*group).is_some_and(|ids| ids.len() > 1))
        })
        .map(|(group, _)| group.to_owned())
        .collect()
}

fn call_index<'a>(
    records: &'a Records,
    members: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, BTreeSet<&'a str>>> {
    let mut calls = BTreeMap::<String, BTreeSet<&str>>::new();
    let mut owners = BTreeMap::new();
    for (id, record) in records {
        if record["type"] == "assistant" {
            for call in blocks(record, "tool_use", "id") {
                if let Some(previous) = owners.insert(call, &members[id]) {
                    ensure!(
                        previous == &members[id],
                        "Claude tool call identity conflicts across responses"
                    );
                }
                calls.entry(members[id].clone()).or_default().insert(call);
            }
        }
    }
    Ok(calls)
}

fn quotient(
    records: &Records,
    members: &BTreeMap<String, String>,
    selector: Option<&str>,
) -> Result<BTreeMap<Option<String>, String>> {
    let mut parents = BTreeMap::<String, Option<String>>::new();
    for (id, record) in records {
        let group = &members[id];
        let parent = record["parentUuid"].as_str().map(|id| members[id].clone());
        if parent.as_ref() == Some(group) {
            continue;
        }
        if let Some(previous) = parents.insert(group.clone(), parent.clone()) {
            ensure!(previous == parent, AMBIGUOUS);
        }
    }
    if let Some(selector) = selector {
        let group = members
            .get(selector)
            .context("Claude authoritative leaf identity missing")?;
        let selected = selected_groups(group, &parents)?;
        parents.retain(|group, _| selected.contains(group));
    }
    let mut children = BTreeMap::new();
    for (group, parent) in parents {
        ensure!(children.insert(parent, group).is_none(), AMBIGUOUS);
    }
    Ok(children)
}

fn selected_groups(
    group: &str,
    parents: &BTreeMap<String, Option<String>>,
) -> Result<BTreeSet<String>> {
    let mut selected = BTreeSet::new();
    let mut current = Some(group);
    while let Some(group) = current {
        ensure!(
            selected.insert(group.into()),
            "Claude authoritative batch ancestry cycles"
        );
        current = parents
            .get(group)
            .context("Claude authoritative batch parent missing")?
            .as_deref();
    }
    Ok(selected)
}

fn expand(
    ids: &[String],
    records: &Records,
    members: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let positions: BTreeMap<_, _> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    let mut children = BTreeMap::<&str, Vec<&str>>::new();
    let mut ready = BTreeSet::new();
    for id in ids {
        let parent = records[id]["parentUuid"].as_str();
        if let Some(parent) = parent.filter(|parent| members[*parent] == members[id]) {
            children.entry(parent).or_default().push(id);
        } else {
            ready.insert((positions[id.as_str()], id.as_str()));
        }
    }
    let mut output = Vec::new();
    while let Some((_, id)) = ready.pop_first() {
        output.push(id.into());
        for child in children.get(id).into_iter().flatten() {
            ready.insert((positions[child], *child));
        }
    }
    ensure!(
        output.len() == ids.len(),
        "Claude response batch ancestry contains a cycle"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(id: &str, parent: &str, response: &str, call: &str) -> Value {
        json!({"uuid":id,"parentUuid":parent,"sessionId":"owned","type":"assistant","message":{"id":response,"role":"assistant","content":[{"type":"tool_use","id":call}]}})
    }
    fn result(id: &str, parent: &str, call: &str) -> Value {
        json!({"uuid":id,"parentUuid":parent,"sessionId":"owned","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":call}]}})
    }
    fn prompt(id: &str, parent: Option<&str>) -> Value {
        json!({"uuid":id,"parentUuid":parent,"sessionId":"owned","type":"user","message":{"role":"user","content":""}})
    }
    fn repair(records: Vec<Value>) -> Result<Vec<Value>> {
        super::super::reconstruct(records, "owned")
    }
    fn fixture() -> Vec<Value> {
        vec![
            prompt("root", None),
            assistant("a1", "root", "response", "call1"),
            result("r1", "a1", "call1"),
            assistant("a2", "a1", "response", "call2"),
            result("r2", "a2", "call2"),
            prompt("next", Some("r2")),
        ]
    }
    #[test]
    fn native_parallel_response_keeps_every_result_and_source_order() {
        let repaired = repair(fixture()).unwrap();
        assert_eq!(
            repaired
                .iter()
                .map(|r| r["uuid"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["root", "a1", "r1", "a2", "r2", "next"]
        );
        assert_eq!(
            repaired
                .iter()
                .filter(|r| !blocks(r, "tool_result", "tool_use_id").is_empty())
                .count(),
            2
        );
    }
    #[test]
    fn ordinary_linear_response_chunks_without_tools_still_work() {
        let mut records = vec![
            prompt("root", None),
            assistant("a1", "root", "response", "call1"),
            assistant("a2", "a1", "response", "call2"),
        ];
        for record in &mut records[1..] {
            record["message"]["content"] = json!([{"type":"text"}]);
        }
        assert_eq!(repair(records).unwrap().len(), 3);
    }
    #[test]
    fn batch_result_can_reference_another_chunk_of_exact_response() {
        let mut records = fixture();
        records[4]["message"]["content"][0]["tool_use_id"] = json!("call1");
        assert!(repair(records).is_ok());
    }
    #[test]
    fn result_before_exact_retained_call_is_explicitly_refused_not_silently_projected() {
        let mut records = fixture();
        records[2]["message"]["content"][0]["tool_use_id"] = json!("call2");
        let error = repair(records).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("precedes its retained native call")
        );
    }
    #[test]
    fn auxiliary_bridge_between_exact_response_chunks_preserves_ancestry() {
        let mut records = fixture();
        records.insert(
            3,
            json!({"type":"attachment","uuid":"bridge","parentUuid":"a1","sessionId":"owned"}),
        );
        records[4]["parentUuid"] = json!("bridge");
        let repaired = repair(records).unwrap();
        assert!(repaired.iter().any(|record| record["uuid"] == "bridge"));
    }
    #[test]
    fn native_unbranched_meta_tails_are_kept_but_real_prompts_are_not_tails() {
        let mut records = fixture();
        for id in ["meta1", "meta2"] {
            let mut tail = prompt(id, Some("a1"));
            tail["isMeta"] = json!(true);
            records.push(tail);
        }
        assert_eq!(repair(records.clone()).unwrap().len(), 8);
        records.last_mut().unwrap()["isMeta"] = json!(false);
        assert!(repair(records).is_err());
    }
    fn preservation(records: &mut Vec<Value>, ids: &[&str]) {
        records.push(json!({"type":"system","subtype":"compact_boundary","uuid":"boundary","parentUuid":null,"sessionId":"owned","compactMetadata":{"preservedMessages":{"anchorUuid":"boundary","uuids":ids}}}));
    }
    #[test]
    fn compaction_rethreaded_results_keep_verified_original_calls() {
        for preserved in [vec!["a1", "r1", "a2", "r2"], vec!["r1", "a2", "r2"]] {
            let mut records = fixture();
            records.pop();
            preservation(&mut records, &preserved);
            let repaired = repair(records).unwrap();
            assert_eq!(
                repaired
                    .iter()
                    .filter(|record| !blocks(record, "tool_result", "tool_use_id").is_empty())
                    .count(),
                2
            );
        }
    }
    #[test]
    fn compaction_never_attests_an_unmatched_retained_result() {
        let mut records = fixture();
        records.pop();
        records[2]["message"]["content"][0]["tool_use_id"] = json!("unknown");
        preservation(&mut records, &["r1", "a2", "r2"]);
        assert!(repair(records).is_err());
    }
    #[test]
    fn unrelated_branches_unmatched_results_and_owner_conflicts_refused() {
        let mut branch = fixture();
        branch.push(prompt("fork", Some("r1")));
        let mut unmatched = fixture();
        unmatched[2]["message"]["content"][0]["tool_use_id"] = json!("unknown");
        let mut owner = fixture();
        owner[3]["agentId"] = json!("other");
        let mut response = fixture();
        response[3]["message"]["id"] = json!("other");
        for records in [branch, unmatched, owner, response] {
            assert!(repair(records).is_err());
        }
    }
    #[test]
    fn repeated_response_across_real_user_prompt_does_not_merge_turns() {
        let mut records = fixture();
        records.insert(3, prompt("intervening", Some("r1")));
        records[4]["parentUuid"] = json!("intervening");
        assert!(repair(records).is_err());
    }
    #[test]
    fn internal_cycles_and_missing_ancestors_refused() {
        let mut cycle = fixture();
        cycle[1]["parentUuid"] = json!("a2");
        let mut missing = fixture();
        missing[1]["parentUuid"] = json!("gone");
        for records in [cycle, missing] {
            assert!(repair(records).is_err());
        }
    }
    #[test]
    fn known_native_metadata_is_not_a_parent_record() {
        for kind in [
            "frame-link",
            "artifact-comment-monitor",
            "artifact-autoreact-ledger",
        ] {
            assert!(!super::super::transcript(&json!({"type":kind})).unwrap());
        }
    }
}
