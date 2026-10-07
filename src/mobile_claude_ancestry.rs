//! Strict subset of Claude 2.1.274's iOs/aOs native compaction relinker.
//! Never choose an append-order leaf or use the native timestamp fallback.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
#[path = "mobile_claude_batches.rs"]
mod batches;

pub(super) fn reconstruct(records: Vec<Value>, identity: &str) -> Result<Vec<Value>> {
    let selector = provider_selector(&records, identity);
    let mut order = Vec::new();
    let mut map = BTreeMap::new();
    for record in records {
        if record["sessionId"].as_str() != Some(identity) || record["isSidechain"] == true {
            continue;
        }
        if !transcript(&record)? {
            continue;
        }
        let id = record["uuid"]
            .as_str()
            .context("Claude message UUID missing")?
            .to_owned();
        ensure!(
            !id.is_empty() && id.len() <= 512,
            "Invalid Claude message identity"
        );
        let parent = record
            .get("parentUuid")
            .context("Claude native parent identity is missing")?;
        ensure!(
            parent.is_null() || parent.is_string(),
            "Invalid Claude native parent identity"
        );
        ensure!(
            !map.contains_key(&id),
            "Claude repeated message identity needs native reconstruction; read it in the original terminal"
        );
        order.push(id.clone());
        map.insert(id, record);
    }
    let result_sources = batches::result_sources(&map);
    compact(&order, &mut map)?;
    validate_parents(&map)?;
    validate_cycles(&map)?;
    let selector = selector.and_then(|selector| conversational_selector(selector, &map));
    prune_attachments(&mut map);
    batches::chain(map, &order, &result_sources, selector)
}

#[derive(Default)]
struct ProviderSelector {
    main: Option<(String, usize)>,
    pointer: Option<(Value, usize, Option<usize>)>,
    conflict: bool,
}

impl ProviderSelector {
    fn observe(&mut self, record: &Value, index: usize) {
        match record["type"].as_str() {
            Some("user" | "assistant" | "system" | "attachment") => {
                self.main = record["uuid"].as_str().map(|id| (id.into(), index));
                self.conflict = false;
            }
            Some("last-prompt") => {
                if let Some(pointer) = record.get("leafUuid") {
                    let generation = self.main.as_ref().map(|(_, index)| *index);
                    if let Some((previous, _, previous_generation)) = &self.pointer {
                        self.conflict |= *previous_generation == generation && previous != pointer;
                    }
                    self.pointer = Some((pointer.clone(), index, generation));
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Result<Option<String>> {
        let Some((pointer, index, _)) = self.pointer else {
            return Ok(None);
        };
        let pointer = pointer
            .as_str()
            .context("Invalid Claude authoritative leaf selector")?;
        ensure!(
            !self.conflict,
            "Conflicting Claude authoritative leaf selectors"
        );
        let (main, main_index) = self
            .main
            .context("Claude authoritative leaf has no owned transcript")?;
        ensure!(
            pointer == main && index > main_index,
            "Claude authoritative leaf selector is stale or mismatched"
        );
        Ok(Some(pointer.into()))
    }
}

fn provider_selector(records: &[Value], identity: &str) -> Result<Option<String>> {
    let mut selector = ProviderSelector::default();
    for (index, record) in records.iter().enumerate() {
        if record["sessionId"].as_str() == Some(identity) && record["isSidechain"] != true {
            selector.observe(record, index);
        }
    }
    selector.finish()
}

fn conversational_selector(
    selector: Option<String>,
    map: &BTreeMap<String, Value>,
) -> Result<Option<String>> {
    let Some(mut id) = selector else {
        return Ok(None);
    };
    let mut seen = BTreeSet::new();
    loop {
        ensure!(
            seen.insert(id.clone()),
            "Claude authoritative leaf ancestry cycles"
        );
        let record = map
            .get(&id)
            .context("Claude authoritative leaf is missing after compaction")?;
        if record["type"] != "attachment" {
            return Ok(Some(id));
        }
        id = record["parentUuid"]
            .as_str()
            .context("Claude authoritative leaf lacks conversational ancestry")?
            .into();
    }
}

pub(super) fn transcript(record: &Value) -> Result<bool> {
    let kind = record["type"]
        .as_str()
        .context("Claude record type missing")?;
    if matches!(kind, "user" | "assistant" | "system" | "attachment") {
        ensure!(
            record.get("compactMetadata").is_none() || boundary(record),
            "Unknown Claude compaction schema"
        );
        return Ok(true);
    }
    // Native sri classifies these as metadata, not parent-chain records.
    ensure!(
        matches!(
            kind,
            "progress"
                | "file-history-snapshot"
                | "file-history-delta"
                | "last-prompt"
                | "continued-in"
                | "summary"
                | "custom-title"
                | "ended-by-model"
                | "ai-title"
                | "tag"
                | "relocated"
                | "agent-name"
                | "agent-color"
                | "agent-setting"
                | "pr-link"
                | "bridge-session"
                | "history-suppression"
                | "attribution-snapshot"
                | "mode"
                | "permission-mode"
                | "isolation-latch"
                | "atis-latch"
                | "worktree-state"
                | "cost-state"
                | "queue-operation"
                | "observer-ref"
                | "frame-link"
                | "artifact-comment-monitor"
                | "artifact-autoreact-ledger"
        ),
        "Unsupported Claude history record schema"
    );
    Ok(false)
}

fn boundary(record: &Value) -> bool {
    record["type"] == "system" && record["subtype"] == "compact_boundary"
}

fn compact(order: &[String], map: &mut BTreeMap<String, Value>) -> Result<()> {
    let Some(index) = order.iter().rposition(|id| boundary(&map[id])) else {
        return Ok(());
    };
    let marker = &map[&order[index]];
    let metadata = marker["compactMetadata"]
        .as_object()
        .context("Unknown Claude compaction schema; native relinking requires metadata")?;
    ensure!(
        metadata.keys().all(|key| matches!(
            key.as_str(),
            "trigger"
                | "preTokens"
                | "postTokens"
                | "cumulativeDroppedTokens"
                | "durationMs"
                | "userContext"
                | "messagesSummarized"
                | "precomputed"
                | "preCompactDiscoveredTools"
                | "preservedMessages"
                | "preservedSegment"
        )),
        "Unknown Claude compaction metadata schema"
    );
    let (anchor, preserved) = preserved(metadata, map)?;
    let keep: BTreeSet<_> = preserved.iter().cloned().collect();
    ensure!(
        keep.len() == preserved.len(),
        "Claude preserved identities repeat"
    );
    ensure!(
        preserved.iter().all(|id| map.contains_key(id)),
        "Claude preserved message is missing"
    );
    if let Some(anchor) = &anchor {
        ensure!(
            map.contains_key(anchor) && !keep.contains(anchor),
            "Claude preserved anchor is missing or cyclic"
        );
        ensure!(
            order[index..].contains(anchor),
            "Claude preserved anchor predates its boundary"
        );
        rethread(map, anchor, &preserved);
    }
    let removed: BTreeSet<_> = order[..index]
        .iter()
        .filter(|id| !keep.contains(*id))
        .cloned()
        .collect();
    for id in &removed {
        map.remove(id);
    }
    if let Some(tail) = preserved.last() {
        for record in map.values_mut() {
            if matches!(record["type"].as_str(), Some("user" | "assistant"))
                && record["parentUuid"]
                    .as_str()
                    .is_some_and(|id| removed.contains(id))
            {
                record["parentUuid"] = Value::String(tail.clone());
            }
        }
    }
    Ok(())
}

fn preserved(
    metadata: &serde_json::Map<String, Value>,
    map: &BTreeMap<String, Value>,
) -> Result<(Option<String>, Vec<String>)> {
    if let Some(list) = metadata.get("preservedMessages") {
        let list = list
            .as_object()
            .context("Unknown Claude preservedMessages schema; native relinking unavailable")?;
        ensure!(
            list.keys()
                .all(|key| matches!(key.as_str(), "anchorUuid" | "uuids" | "allUuids")),
            "Unknown Claude preservedMessages schema"
        );
        let ids = list
            .get("uuids")
            .and_then(Value::as_array)
            .context("Claude preserved UUID list missing")?;
        let ids = ids
            .iter()
            .map(|id| {
                id.as_str()
                    .map(str::to_owned)
                    .context("Invalid Claude preserved identity")
            })
            .collect::<Result<Vec<_>>>()?;
        let anchor = list
            .get("anchorUuid")
            .and_then(Value::as_str)
            .context("Claude preserved anchor missing")?
            .to_owned();
        ensure!(
            !ids.is_empty(),
            "Empty Claude preservation schema unsupported"
        );
        return Ok((Some(anchor), ids));
    }
    if let Some(segment) = metadata.get("preservedSegment") {
        return preserved_segment(segment, map);
    }
    ensure!(
        !metadata.keys().any(|key| key.starts_with("preserved")),
        "Unknown Claude preservation schema"
    );
    Ok((None, Vec::new()))
}

fn preserved_segment(
    segment: &Value,
    map: &BTreeMap<String, Value>,
) -> Result<(Option<String>, Vec<String>)> {
    let fields = segment
        .as_object()
        .context("Unknown Claude preservedSegment schema")?;
    ensure!(
        fields
            .keys()
            .all(|key| matches!(key.as_str(), "headUuid" | "tailUuid" | "anchorUuid")),
        "Unknown Claude preservedSegment schema"
    );
    let head = segment["headUuid"]
        .as_str()
        .context("Claude preserved head missing")?;
    let mut current = segment["tailUuid"]
        .as_str()
        .context("Claude preserved tail missing")?;
    let anchor = segment["anchorUuid"]
        .as_str()
        .context("Claude preserved anchor missing")?;
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    loop {
        ensure!(
            seen.insert(current.to_owned()),
            "Claude preservation walk cycles"
        );
        let record = map
            .get(current)
            .context("Claude preservation walk has a missing ancestor")?;
        ids.push(current.to_owned());
        if current == head {
            break;
        }
        current = record["parentUuid"]
            .as_str()
            .context("Claude preservation walk does not reach its head")?;
    }
    ids.reverse();
    Ok((Some(anchor.to_owned()), ids))
}

fn rethread(map: &mut BTreeMap<String, Value>, anchor: &str, ids: &[String]) {
    let mut parent = anchor;
    for id in ids {
        if let Some(record) = map.get_mut(id) {
            record["parentUuid"] = Value::String(parent.to_owned());
        }
        parent = id;
    }
    for (id, record) in map.iter_mut() {
        if record["parentUuid"].as_str() == Some(anchor) && id != &ids[0] {
            record["parentUuid"] = Value::String(parent.to_owned());
        }
    }
}

fn validate_parents(map: &BTreeMap<String, Value>) -> Result<()> {
    for record in map.values() {
        if let Some(parent) = record["parentUuid"].as_str() {
            ensure!(
                map.contains_key(parent),
                "Claude history has a missing native ancestor; read it in the original terminal"
            );
        }
    }
    Ok(())
}

fn validate_cycles(map: &BTreeMap<String, Value>) -> Result<()> {
    let mut done = BTreeSet::new();
    for id in map.keys() {
        let mut path = BTreeSet::new();
        let mut current = Some(id.as_str());
        while let Some(id) = current {
            if done.contains(id) {
                break;
            }
            ensure!(
                path.insert(id.to_owned()),
                "Claude native ancestry contains a cycle"
            );
            current = map[id]["parentUuid"].as_str();
        }
        done.extend(path);
    }
    Ok(())
}

fn prune_attachments(map: &mut BTreeMap<String, Value>) {
    let mut counts = BTreeMap::<String, usize>::new();
    for record in map.values() {
        if let Some(parent) = record["parentUuid"].as_str() {
            *counts.entry(parent.into()).or_default() += 1;
        }
    }
    let mut leaves: Vec<_> = map
        .iter()
        .filter(|(id, record)| record["type"] == "attachment" && !counts.contains_key(*id))
        .map(|(id, _)| id.clone())
        .collect();
    while let Some(id) = leaves.pop() {
        if let Some(record) = map.remove(&id) {
            if let Some(parent) = record["parentUuid"].as_str() {
                if let Some(count) = counts.get_mut(parent) {
                    *count -= 1;
                    if *count == 0
                        && map
                            .get(parent)
                            .is_some_and(|record| record["type"] == "attachment")
                    {
                        leaves.push(parent.into());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn message(id: &str, parent: Option<&str>) -> Value {
        json!({"type":"user","uuid":id,"parentUuid":parent,"sessionId":"owned","message":{"role":"user","content":id}})
    }
    fn marker(id: &str, metadata: Value) -> Value {
        json!({"type":"system","subtype":"compact_boundary","uuid":id,"parentUuid":null,"sessionId":"owned","compactMetadata":metadata})
    }
    fn ids(records: Vec<Value>) -> Vec<String> {
        reconstruct(records, "owned")
            .unwrap()
            .iter()
            .map(|record| record["uuid"].as_str().unwrap().into())
            .collect()
    }
    #[test]
    fn authoritative_provider_leaf_selects_exact_branch_not_latest_fallback() {
        let mut records = vec![
            message("root", None),
            message("old", Some("root")),
            message("selected", Some("root")),
        ];
        assert!(reconstruct(records.clone(), "owned").is_err());
        records.push(json!({"type":"last-prompt","sessionId":"owned","leafUuid":"selected"}));
        assert_eq!(ids(records), ["root", "selected"]);
    }
    #[test]
    fn stale_dangling_conflicting_and_missing_provider_selectors_refused() {
        let base = vec![
            message("root", None),
            message("old", Some("root")),
            message("selected", Some("root")),
        ];
        let pointer = |id: &str| json!({"type":"last-prompt","sessionId":"owned","leafUuid":id});
        let mut stale = base.clone();
        stale.insert(2, pointer("selected"));
        let mut dangling = base.clone();
        dangling.push(pointer("missing"));
        let mut conflict = base.clone();
        conflict.extend([pointer("old"), pointer("selected")]);
        let mut missing = base.clone();
        missing.push(json!({"type":"last-prompt","sessionId":"owned"}));
        let mut foreign = base;
        foreign.push(json!({"type":"last-prompt","sessionId":"other","leafUuid":"selected"}));
        for records in [stale, dangling, conflict, missing, foreign] {
            assert!(reconstruct(records, "owned").is_err());
        }
    }
    #[test]
    fn stale_provider_metadata_is_not_needed_to_resolve_unique_ancestry() {
        let records = vec![
            message("root", None),
            json!({"type":"last-prompt","sessionId":"owned","leafUuid":"root"}),
            message("next", Some("root")),
        ];
        assert_eq!(ids(records), ["root", "next"]);
    }
    #[test]
    fn native_list_rethreads_physically_old_preserved_messages_after_summary() {
        assert_eq!(
            ids(vec![
                message("old", None),
                message("keep1", Some("old")),
                message("keep2", Some("keep1")),
                marker(
                    "boundary",
                    json!({"preservedMessages":{"anchorUuid":"summary","uuids":["keep1","keep2"],"allUuids":["old","keep1","keep2"]}})
                ),
                message("summary", Some("boundary")),
                message("new", Some("summary"))
            ]),
            ["boundary", "summary", "keep1", "keep2", "new"]
        );
    }
    #[test]
    fn native_segment_walk_and_boundary_anchor_match_list_order() {
        assert_eq!(
            ids(vec![
                message("old", None),
                message("keep1", Some("old")),
                message("keep2", Some("keep1")),
                marker(
                    "boundary",
                    json!({"preservedSegment":{"anchorUuid":"boundary","headUuid":"keep1","tailUuid":"keep2"}})
                ),
                message("new", Some("boundary"))
            ]),
            ["boundary", "keep1", "keep2", "new"]
        );
    }
    #[test]
    fn latest_full_compaction_discards_prior_preserved_context() {
        assert_eq!(
            ids(vec![
                message("old", None),
                marker(
                    "first",
                    json!({"preservedMessages":{"anchorUuid":"first","uuids":["old"]}})
                ),
                message("middle", Some("first")),
                marker("latest", json!({"trigger":"auto","preTokens":123})),
                message("summary", Some("latest")),
                message("new", Some("summary"))
            ]),
            ["latest", "summary", "new"]
        );
    }
    #[test]
    fn malformed_compaction_and_conversation_branches_remain_refused() {
        for metadata in [
            json!({"preservedMessages":["old"]}),
            json!({"preservedMessages":{"anchorUuid":"boundary","uuids":["missing"]}}),
            json!({"preservedMessages":{"anchorUuid":"boundary","uuids":["old","old"]}}),
            json!({"preservedSegment":{"anchorUuid":"boundary","headUuid":"missing","tailUuid":"old"}}),
            json!({"preservedFuture":{"uuids":["old"]}}),
            json!({"unknownRouting":{"uuids":["old"]}}),
        ] {
            assert!(
                reconstruct(
                    vec![
                        message("old", None),
                        marker("boundary", metadata),
                        message("new", Some("boundary"))
                    ],
                    "owned"
                )
                .is_err()
            );
        }
        assert!(
            reconstruct(
                vec![
                    message("root", None),
                    message("one", Some("root")),
                    message("other", Some("root"))
                ],
                "owned"
            )
            .is_err()
        );
    }
    #[test]
    fn native_progress_does_not_create_a_conversation_branch() {
        assert_eq!(
            ids(vec![
                message("root", None),
                json!({"type":"progress","uuid":"progress","parentUuid":"root","sessionId":"owned"}),
                message("new", Some("root"))
            ]),
            ["root", "new"]
        );
    }
}
