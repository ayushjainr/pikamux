//! Two-pass frozen-source reader: bounded records and compact ancestry metadata.
use super::{Cursor, PAGE, RECORD, claude_ancestry, claude_projection};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
};

const INDEX_BYTES: usize = 128 * 1024 * 1024;
// Additional charge for ancestry maps, IDs, vector slots and reconstruction clones.
const NODE_OVERHEAD: usize = 1024;

pub(super) fn page(
    paths: &crate::paths::Paths,
    file: &mut File,
    state: &mut Cursor,
    initial: bool,
) -> Result<(Vec<Value>, bool)> {
    let (metadata, source_hash) = scan(file, state.snapshot, &state.id)?;
    check_frozen_hash(
        initial,
        state.claude_source_hash.as_deref(),
        &source_hash,
        "Claude frozen history changed; reopen it",
    )?;
    let ordered = claude_ancestry::reconstruct(metadata, &state.id)?;
    let mut projector = claude_projection::Projector::new(paths, &state.id)?;
    let target = if initial {
        None
    } else {
        Some(
            state
                .claude_before
                .context("Claude history cursor predates native reconstruction; reopen it")?,
        )
    };
    let (page, count, canonical) = project_window(file, &ordered, &mut projector, target)?;
    check_frozen_hash(
        initial,
        state.claude_items_hash.as_deref(),
        &canonical,
        "Claude native projection changed; reopen it",
    )?;
    ensure!(
        target.is_none_or(|end| end <= count),
        "Claude logical history boundary changed; reopen it"
    );
    ensure!(
        source_digest(file, state.snapshot)? == source_hash,
        "Claude frozen history changed during read; reopen it"
    );
    let end = target.unwrap_or(count);
    let start = end.saturating_sub(PAGE);
    state.claude_before = Some(start);
    state.claude_items_hash = Some(canonical);
    state.claude_source_hash = Some(source_hash);
    Ok((page, start > 0))
}

fn project_window(
    file: &mut File,
    ordered: &[Value],
    projector: &mut claude_projection::Projector,
    target: Option<usize>,
) -> Result<(Vec<Value>, usize, String)> {
    let mut page = VecDeque::new();
    let mut count = 0usize;
    let mut digest = Sha256::new();
    digest.update(b"[");
    for metadata in ordered {
        let record = read_record(file, metadata)?;
        for entry in projector.entries(&record)? {
            if count > 0 {
                digest.update(b",");
            }
            digest.update(serde_json::to_vec(&entry)?);
            count += 1;
            if target.is_none_or(|end| count <= end) {
                page.push_back(entry);
                if page.len() > PAGE {
                    page.pop_front();
                }
            }
        }
    }
    digest.update(b"]");
    Ok((
        page.into_iter().collect(),
        count,
        format!("{:x}", digest.finalize()),
    ))
}

fn check_frozen_hash(
    initial: bool,
    expected: Option<&str>,
    actual: &str,
    reason: &str,
) -> Result<()> {
    ensure!(initial || expected == Some(actual), "{reason}");
    Ok(())
}

fn scan(file: &mut File, snapshot: u64, identity: &str) -> Result<(Vec<Value>, String)> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file.take(snapshot));
    let mut records = Vec::new();
    let mut used = 0usize;
    let mut offset = 0u64;
    let mut digest = Sha256::new();
    loop {
        let mut bytes = Vec::new();
        let read = reader
            .by_ref()
            .take((RECORD + 2) as u64)
            .read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        ensure!(
            read <= RECORD + 1,
            "Claude individual history record exceeds 256 KiB; the total history size is not the limit"
        );
        digest.update(&bytes);
        if !bytes.ends_with(b"\n") {
            break;
        }
        if read > 1 {
            let record: Value = serde_json::from_slice(&bytes)
                .context("Malformed durable Claude history record")?;
            if record["sessionId"].as_str() == Some(identity)
                && record["isSidechain"] != true
                && claude_ancestry::transcript(&record)?
            {
                let mut compact = metadata(&record, offset, read as u64);
                compact["_pikaDigest"] = Value::String(format!("{:x}", Sha256::digest(&bytes)));
                used = used
                    .checked_add(
                        retained_charge(&compact)?
                            .checked_mul(4)
                            .context("Claude metadata allocation overflow")?
                            + NODE_OVERHEAD,
                    )
                    .context("Claude ancestry metadata size overflow")?;
                ensure!(
                    used <= INDEX_BYTES,
                    "Claude ancestry metadata exceeds its bounded memory budget; total transcript bytes are not the limit"
                );
                records.push(compact);
            }
        }
        offset += read as u64;
    }
    Ok((records, format!("{:x}", digest.finalize())))
}

fn retained_charge(value: &Value) -> Result<usize> {
    // Charge recursive allocation, not JSON encoding: tiny nested objects can
    // occupy substantially more heap than their serialized representation.
    let mut bytes = std::mem::size_of::<Value>();
    match value {
        Value::String(text) => bytes += text.capacity(),
        Value::Array(values) => {
            bytes += values.capacity() * std::mem::size_of::<Value>();
            for child in values {
                bytes = bytes
                    .checked_add(retained_charge(child)?)
                    .context("Claude metadata allocation overflow")?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                bytes = bytes
                    .checked_add(key.capacity() + 256 + retained_charge(child)?)
                    .context("Claude metadata allocation overflow")?;
            }
        }
        _ => {}
    }
    Ok(bytes)
}

fn metadata(record: &Value, offset: u64, length: u64) -> Value {
    let mut result = json!({"_pikaOffset":offset,"_pikaLength":length});
    for key in [
        "type",
        "uuid",
        "parentUuid",
        "sessionId",
        "isSidechain",
        "isMeta",
        "subtype",
        "compactMetadata",
    ] {
        if let Some(value) = record.get(key) {
            result[key] = value.clone();
        }
    }
    result
}

fn read_record(file: &mut File, metadata: &Value) -> Result<Value> {
    let offset = metadata["_pikaOffset"]
        .as_u64()
        .context("Claude record offset missing")?;
    let length = metadata["_pikaLength"]
        .as_u64()
        .context("Claude record length missing")?;
    ensure!(
        length <= (RECORD + 1) as u64,
        "Invalid Claude record read bound"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    ensure!(
        metadata["_pikaDigest"].as_str() == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
        "Claude frozen record changed during read; reopen it"
    );
    serde_json::from_slice(&bytes).context("Claude frozen record changed during read")
}

fn source_digest(file: &mut File, snapshot: u64) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = file.take(snapshot);
    let mut digest = Sha256::new();
    let mut bytes = [0; 16 * 1024];
    let mut read = 0u64;
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
        read += count as u64;
    }
    ensure!(
        read == snapshot,
        "Claude frozen history was truncated; reopen it"
    );
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    #[test]
    fn retained_metadata_charge_measured_for_five_thousand_nodes() {
        let mut file = tempfile::tempfile().unwrap();
        for index in 0..5000 {
            writeln!(file,"{}",json!({"type":"user","uuid":format!("item-{index}"),"parentUuid":if index==0 {None}else{Some(format!("item-{}",index-1))},"sessionId":ID,"message":{"role":"user","content":"short"}})).unwrap();
        }
        let size = file.metadata().unwrap().len();
        let (records, _) = scan(&mut file, size, ID).unwrap();
        let charged: usize = records
            .iter()
            .map(|record| retained_charge(record).unwrap() * 4 + NODE_OVERHEAD)
            .sum();
        assert_eq!(records.len(), 5000);
        assert!(charged < INDEX_BYTES);
        eprintln!(
            "metadata measurement: {} source bytes, {} nodes, {} conservative charged bytes (not total RSS)",
            size,
            records.len(),
            charged
        );
    }

    #[test]
    fn reread_refuses_changed_record_even_if_source_is_restored_later() {
        let mut file = tempfile::tempfile().unwrap();
        writeln!(file,"{}",json!({"type":"user","uuid":"one","parentUuid":null,"sessionId":ID,"message":{"role":"user","content":"original"}})).unwrap();
        let size = file.metadata().unwrap().len();
        let (records, original_hash) = scan(&mut file, size, ID).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b" ").unwrap();
        assert!(
            read_record(&mut file, &records[0])
                .unwrap_err()
                .to_string()
                .contains("frozen record changed")
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"{").unwrap();
        assert_eq!(source_digest(&mut file, size).unwrap(), original_hash);
    }

    #[test]
    fn nested_metadata_is_charged_and_budget_excess_never_returns_partial_index() {
        let small = json!({"a":[{}, {}, {}, {}]});
        assert!(retained_charge(&small).unwrap() > serde_json::to_vec(&small).unwrap().len());
        let mut file = tempfile::tempfile().unwrap();
        let payload = "x".repeat(200 * 1024);
        for index in 0..180 {
            writeln!(file,"{}",json!({"type":"system","subtype":"compact_boundary","uuid":format!("boundary-{index}"),"sessionId":ID,"compactMetadata":{"userContext":payload}})).unwrap();
        }
        let size = file.metadata().unwrap().len();
        assert!(
            scan(&mut file, size, ID)
                .unwrap_err()
                .to_string()
                .contains("ancestry metadata exceeds")
        );
    }
}
