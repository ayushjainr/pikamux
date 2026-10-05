//! Project only native transcript records correlated with the private channel journal.
//! The journal does not create or append synthetic history turns.
use super::{item, jsonl_item, text_parts};
use crate::{mobile_claude::channel, model::Provider, paths::Paths};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

type Proofs = BTreeMap<(String, String), channel::HistoryOperation>;

pub(super) fn project(
    paths: &Paths,
    thread: &str,
    records: &[Value],
) -> Result<BTreeMap<String, Vec<Value>>> {
    let requests = history_requests(thread, records);
    let proofs = channel::history_operations(paths, thread, &requests)?;
    let mut fetched = BTreeMap::new();
    let mut projected = BTreeMap::new();
    for record in records {
        let mut entries = record_entries(&proofs, thread, record, &mut fetched)?;
        if !entries.is_empty() {
            if let Some(text) = jsonl_item(record, Provider::Claude, thread)? {
                entries.insert(0, text);
            }
            let id = record["uuid"]
                .as_str()
                .context("Native history record identity missing")?;
            ensure!(
                projected.insert(id.into(), entries).is_none(),
                "Repeated native history identity"
            );
        }
    }
    Ok(projected)
}

fn history_requests(thread: &str, records: &[Value]) -> Vec<(String, String)> {
    records
        .iter()
        .filter(|record| record["sessionId"] == thread && record["isSidechain"] != true)
        .flat_map(|record| {
            record["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter(|part| part["type"] == "tool_use")
        .filter_map(|part| tool_reference(part).map(|(token, operation, _)| (token, operation)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn record_entries(
    proofs: &Proofs,
    thread: &str,
    record: &Value,
    fetched: &mut BTreeMap<String, String>,
) -> Result<Vec<Value>> {
    let role = record["type"].as_str().unwrap_or_default();
    if record["sessionId"] != thread
        || record["isSidechain"] == true
        || !matches!(role, "user" | "assistant")
        || record["message"]["role"] != role
    {
        return Ok(Vec::new());
    }
    let Some(parts) = record["message"]["content"].as_array() else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for part in parts {
        let entry = match (role, part["type"].as_str()) {
            ("assistant", Some("tool_use")) => tool_entry(proofs, part, fetched)?,
            ("user", Some("tool_result")) => fetched_entry(part, fetched)?,
            _ => None,
        };
        if let Some(entry) = entry {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn tool_entry(
    proofs: &Proofs,
    part: &Value,
    fetched: &mut BTreeMap<String, String>,
) -> Result<Option<Value>> {
    let Some((token, operation, tool)) = tool_reference(part) else {
        return Ok(None);
    };
    let Some(proof) = proofs.get(&(token, operation)) else {
        return Ok(None);
    };
    let Some(id) = part["id"].as_str() else {
        return Ok(None);
    };
    if tool == "fetch_message" && proof.fetch_tool_id.as_deref() == Some(id) {
        ensure!(
            fetched.insert(id.into(), proof.text.clone()).is_none(),
            "Repeated native fetch identity"
        );
    } else if tool == "reply" && proof.reply_tool_id.as_deref() == Some(id) {
        if let Some(text) = proof
            .reply
            .as_ref()
            .filter(|text| part["input"]["text"].as_str() == Some(text.as_str()))
        {
            return item(&format!("claude-channel-reply:{id}"), false, text.clone()).map(Some);
        }
    }
    Ok(None)
}

fn tool_reference(part: &Value) -> Option<(String, String, &str)> {
    let (token, tool) = parse_name(part["name"].as_str()?)?;
    let operation = uuid::Uuid::parse_str(part["input"]["operation_id"].as_str()?).ok()?;
    Some((token, operation.to_string(), tool))
}

fn fetched_entry(part: &Value, fetched: &mut BTreeMap<String, String>) -> Result<Option<Value>> {
    let Some(id) = part["tool_use_id"].as_str() else {
        return Ok(None);
    };
    let Some(expected) = fetched.remove(id) else {
        return Ok(None);
    };
    if part["is_error"] == true
        || text_parts(&part["content"]).as_deref() != Some(expected.as_str())
    {
        return Ok(None);
    }
    item(&format!("claude-channel-user:{id}"), true, expected).map(Some)
}

fn parse_name(name: &str) -> Option<(String, &str)> {
    let (token, tool) = name.strip_prefix("mcp__pika_")?.split_once("__")?;
    let parsed = uuid::Uuid::parse_str(token).ok()?;
    if parsed.simple().to_string() != token || !matches!(tool, "fetch_message" | "reply") {
        return None;
    }
    Some((parsed.to_string(), tool))
}
