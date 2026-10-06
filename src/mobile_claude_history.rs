//! Project only native transcript records correlated with the private channel journal.
//! The journal does not create or append synthetic history turns.
use super::{item, jsonl_item, text_parts};
use crate::{mobile_claude::channel, model::Provider, paths::Paths};
use anyhow::{Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) struct Projector {
    proofs: channel::HistoricalSnapshot,
    fetched: BTreeMap<String, String>,
    thread: String,
}

impl Projector {
    pub(super) fn new(paths: &Paths, thread: &str) -> Result<Self> {
        Ok(Self {
            proofs: channel::HistoricalSnapshot::open(paths, thread)?,
            fetched: BTreeMap::new(),
            thread: thread.into(),
        })
    }

    pub(super) fn entries(&mut self, record: &Value) -> Result<Vec<Value>> {
        let mut entries = Vec::new();
        if record["message"]["role"] == record["type"] {
            for part in record["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let entry = match (record["type"].as_str(), part["type"].as_str()) {
                    (Some("assistant"), Some("tool_use")) => self.tool(part)?,
                    (Some("user"), Some("tool_result")) => self.fetched(part)?,
                    _ => None,
                };
                if let Some(entry) = entry {
                    entries.push(entry);
                }
            }
        }
        if let Some(text) = jsonl_item(record, Provider::Claude, &self.thread)? {
            entries.insert(0, text);
        }
        Ok(entries)
    }

    fn tool(&mut self, part: &Value) -> Result<Option<Value>> {
        let Some((token, operation, tool)) = tool_reference(part) else {
            return Ok(None);
        };
        let Some(proof) = self.proofs.operation(&token, &operation)? else {
            return Ok(None);
        };
        let Some(id) = part["id"].as_str() else {
            return Ok(None);
        };
        if tool == "fetch_message" && proof.fetch_tool_id.as_deref() == Some(id) {
            ensure!(
                self.fetched.insert(id.into(), hash(&proof.text)).is_none(),
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

    fn fetched(&mut self, part: &Value) -> Result<Option<Value>> {
        let Some(id) = part["tool_use_id"].as_str() else {
            return Ok(None);
        };
        let Some(expected) = self.fetched.remove(id) else {
            return Ok(None);
        };
        let Some(text) = text_parts(&part["content"]) else {
            return Ok(None);
        };
        if part["is_error"] == true || hash(&text) != expected {
            return Ok(None);
        }
        item(&format!("claude-channel-user:{id}"), true, text).map(Some)
    }
}

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn tool_reference(part: &Value) -> Option<(String, String, &str)> {
    let (token, tool) = parse_name(part["name"].as_str()?)?;
    let operation = uuid::Uuid::parse_str(part["input"]["operation_id"].as_str()?).ok()?;
    Some((token, operation.to_string(), tool))
}

fn parse_name(name: &str) -> Option<(String, &str)> {
    let (token, tool) = name.strip_prefix("mcp__pika_")?.split_once("__")?;
    let parsed = uuid::Uuid::parse_str(token).ok()?;
    if parsed.simple().to_string() != token || !matches!(tool, "fetch_message" | "reply") {
        return None;
    }
    Some((parsed.to_string(), tool))
}
