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
    prepared: BTreeMap<String, (String, String, String)>,
    thread: String,
}

impl Projector {
    pub(super) fn new(paths: &Paths, thread: &str) -> Result<Self> {
        Ok(Self {
            proofs: channel::HistoricalSnapshot::open(paths, thread)?,
            fetched: BTreeMap::new(),
            prepared: BTreeMap::new(),
            thread: thread.into(),
        })
    }

    /// Register an exact original-source fetch pruned by native compaction.
    /// This emits nothing: only a surviving, matching native result can become
    /// a user item. Repeated preparation cannot resurrect a consumed result.
    pub(super) fn prepare_fetch(&mut self, part: &Value) -> Result<()> {
        if part["type"] != "tool_use" {
            return Ok(());
        }
        let Some((token, operation, "fetch_message")) = tool_reference(part) else {
            return Ok(());
        };
        let Some(proof) = self.proofs.operation(&token, &operation)? else {
            return Ok(());
        };
        let Some(id) = part["id"].as_str() else {
            return Ok(());
        };
        if proof.fetch_tool_id.as_deref() != Some(id) {
            return Ok(());
        }
        let evidence = (token, operation, hash(&proof.text));
        if let Some(previous) = self.prepared.get(id) {
            ensure!(
                previous == &evidence,
                "Conflicting prepared native fetch identity"
            );
            return Ok(());
        }
        ensure!(
            !self.fetched.contains_key(id),
            "Repeated native fetch identity"
        );
        self.fetched.insert(id.into(), evidence.2.clone());
        self.prepared.insert(id.into(), evidence);
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;

    const THREAD: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TEXT: &str = "  preserved phone input\nwith literal whitespace  ";

    fn fixture() -> (tempfile::TempDir, Projector, Value, Value) {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path();
        let paths = Paths {
            config_dir: p.join("config"),
            config: p.join("config/config.json"),
            state_dir: p.join("state"),
            database: p.join("state/pika.db"),
            codex_home: p.join("codex"),
            claude_home: p.join("claude"),
            opencode_data_home: p.join("oc"),
            opencode_config_home: p.join("oc-config"),
            muse_data_home: p.join("muse"),
            muse_config_home: p.join("muse-config"),
        };
        let store = Store::from_paths(&paths);
        store.initialize().unwrap();
        let token = uuid::Uuid::new_v4();
        let operation = uuid::Uuid::new_v4().to_string();
        store
            .set_meta(&format!("claude-channel:{token}:thread"), THREAD)
            .unwrap();
        store
            .set_meta(
                &format!("claude-channel:{token}:operation:{operation}"),
                &json!({
                    "text":TEXT,"released":true,"reply":"removed reply must stay removed",
                    "attestation":null,"fetch_tool_id":"toolu_fetch","reply_tool_id":"toolu_reply"
                })
                .to_string(),
            )
            .unwrap();
        let call = json!({"type":"tool_use","id":"toolu_fetch","name":format!("mcp__pika_{}__fetch_message",token.simple()),"input":{"operation_id":operation}});
        let result = json!({"type":"user","uuid":"preserved-result","sessionId":THREAD,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fetch","content":[{"type":"text","text":TEXT}]}]}});
        (temp, Projector::new(&paths, THREAD).unwrap(), call, result)
    }

    #[test]
    fn pruned_fetch_preparation_requires_actual_result_and_does_not_replay() {
        let (_temp, mut projector, call, result) = fixture();
        assert!(projector.entries(&result).unwrap().is_empty());
        projector.prepare_fetch(&call).unwrap();
        projector.prepare_fetch(&call).unwrap();
        let entries = projector.entries(&result).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "claude-channel-user:toolu_fetch");
        assert_eq!(entries[0]["text"], TEXT);
        projector.prepare_fetch(&call).unwrap();
        assert!(projector.entries(&result).unwrap().is_empty());
    }

    #[test]
    fn pruned_fetch_preparation_rejects_mismatched_identity_and_never_prepares_reply() {
        for variant in 0..5 {
            let (_temp, mut projector, mut call, result) = fixture();
            match variant {
                0 => call["id"] = json!("toolu_wrong"),
                1 => call["input"]["operation_id"] = json!(uuid::Uuid::new_v4().to_string()),
                2 => {
                    call["name"] = json!(format!(
                        "mcp__pika_{}__fetch_message",
                        uuid::Uuid::new_v4().simple()
                    ))
                }
                3 => {
                    call["name"] = json!(
                        call["name"]
                            .as_str()
                            .unwrap()
                            .replace("__fetch_message", "__reply")
                    );
                    call["id"] = json!("toolu_reply");
                }
                _ => call["type"] = json!("text"),
            }
            projector.prepare_fetch(&call).unwrap();
            assert!(projector.fetched.is_empty());
            assert!(projector.entries(&result).unwrap().is_empty());
        }
    }

    #[test]
    fn prepared_fetch_still_requires_exact_successful_result_content() {
        for failed in [false, true] {
            let (_temp, mut projector, call, mut result) = fixture();
            projector.prepare_fetch(&call).unwrap();
            if failed {
                result["message"]["content"][0]["is_error"] = json!(true);
            } else {
                result["message"]["content"][0]["content"][0]["text"] = json!("different input");
            }
            assert!(projector.entries(&result).unwrap().is_empty());
        }
    }
}
