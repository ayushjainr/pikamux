//! Two-pass frozen-source reader: bounded records and compact ancestry metadata.
use super::{Cursor, PAGE, claude_ancestry, claude_projection};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
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
#[path = "mobile_claude_stream.rs"]
mod stream;
const PAGE_BYTES: usize = super::CLAUDE_PAGE_BYTES;
const VALIDATION_SCRATCH_BYTES: usize = 4 * 1024 * 1024;

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
    let start = end.saturating_sub(page.len());
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
    let mut page_bytes = 2usize;
    let mut digest = Sha256::new();
    digest.update(b"[");
    for metadata in ordered {
        let record = read_record(file, metadata)?;
        for entry in projector.entries(&record)? {
            let encoded = serde_json::to_vec(&entry)?;
            let size = encoded.len() + 1;
            ensure!(
                size + 2 <= PAGE_BYTES,
                "Claude visible message exceeds the 8 MiB encoded mobile history page size"
            );
            if count > 0 {
                digest.update(b",");
            }
            digest.update(&encoded);
            count += 1;
            if target.is_none_or(|end| count <= end) {
                page_bytes += size;
                page.push_back((entry, size));
                while page.len() > PAGE || page_bytes > PAGE_BYTES {
                    let (_, removed_bytes) = page
                        .pop_front()
                        .context("Claude history item exceeds page bounds")?;
                    page_bytes -= removed_bytes;
                }
            }
        }
    }
    digest.update(b"]");
    Ok((
        page.into_iter().map(|(entry, _)| entry).collect(),
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
    let mut records = Vec::new();
    let mut used = 0usize;
    let mut offset = 0u64;
    let mut digest = Sha256::new();
    loop {
        let validated = validate_record(file, offset, snapshot)?;
        let read = validated.length;
        let complete = validated.complete;
        let record_hash = validated.hash;
        if read == 0 {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        hash_range(file, read, &mut digest)?;
        if !complete {
            break;
        }
        if read > 1 {
            validated
                .validation
                .context("Malformed durable Claude history record")?;
            let record = extract_record(file, offset, read, false, &record_hash)?;
            if selected_record(&record, identity)? {
                records.push(charged_metadata(
                    &record,
                    offset,
                    read,
                    record_hash,
                    &mut used,
                )?);
            }
        }
        offset += read;
        if offset == snapshot {
            break;
        }
    }
    Ok((records, format!("{:x}", digest.finalize())))
}

struct ValidatedRecord {
    length: u64,
    complete: bool,
    hash: String,
    validation: serde_json::Result<()>,
}

fn validate_record(file: &mut File, offset: u64, snapshot: u64) -> Result<ValidatedRecord> {
    file.seek(SeekFrom::Start(offset))?;
    let mut scratch = Vec::with_capacity(VALIDATION_SCRATCH_BYTES);
    let mut probe =
        BufReader::new((&mut *file).take((snapshot - offset).min(VALIDATION_SCRATCH_BYTES as u64)));
    probe.read_until(b'\n', &mut scratch)?;
    let complete = scratch.ends_with(b"\n");
    if complete || scratch.len() < VALIDATION_SCRATCH_BYTES {
        let mut de = serde_json::Deserializer::from_slice(&scratch);
        let validation = serde::de::IgnoredAny::deserialize(&mut de).and_then(|_| de.end());
        return Ok(ValidatedRecord {
            length: scratch.len() as u64,
            complete,
            hash: format!("{:x}", Sha256::digest(&scratch)),
            validation,
        });
    }
    // This is a validation scratch budget, never a record limit. Drop it
    // before the unbounded-length, bounded-memory streaming fallback.
    drop(probe);
    drop(scratch);
    validate_streamed_record(file, offset, snapshot)
}

fn validate_streamed_record(
    file: &mut File,
    offset: u64,
    snapshot: u64,
) -> Result<ValidatedRecord> {
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = LineReader::new(BufReader::new((&mut *file).take(snapshot - offset)));
    let mut de = serde_json::Deserializer::from_reader(&mut reader);
    let validation = serde::de::IgnoredAny::deserialize(&mut de).and_then(|_| de.end());
    std::io::copy(&mut reader, &mut std::io::sink())?;
    Ok(ValidatedRecord {
        length: reader.count,
        complete: reader.complete,
        hash: format!("{:x}", reader.digest.finalize()),
        validation,
    })
}

fn selected_record(record: &Value, identity: &str) -> Result<bool> {
    Ok(record["sessionId"].as_str() == Some(identity)
        && record["isSidechain"] != true
        && claude_ancestry::transcript(record)?)
}

fn hash_range(file: &mut File, length: u64, digest: &mut Sha256) -> Result<()> {
    let mut source = file.take(length);
    let mut buffer = [0; 16 * 1024];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            return Ok(());
        }
        digest.update(&buffer[..n]);
    }
}

fn charged_metadata(
    record: &Value,
    offset: u64,
    length: u64,
    hash: String,
    used: &mut usize,
) -> Result<Value> {
    let mut compact = metadata(record, offset, length);
    compact["_pikaDigest"] = Value::String(hash);
    *used = used
        .checked_add(
            retained_charge(&compact)?
                .checked_mul(4)
                .context("Claude metadata allocation overflow")?
                + NODE_OVERHEAD,
        )
        .context("Claude ancestry metadata size overflow")?;
    ensure!(
        *used <= INDEX_BYTES,
        "Claude ancestry metadata exceeds its bounded memory budget; total transcript bytes are not the limit"
    );
    Ok(compact)
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
        offset
            .checked_add(length)
            .is_some_and(|end| end <= file.metadata().map(|m| m.len()).unwrap_or(0)),
        "Invalid Claude record read bound"
    );
    let digest = metadata["_pikaDigest"]
        .as_str()
        .context("Claude validated record digest missing")?;
    // The scan already validated these bytes. Extraction hashes its own exact
    // input and must match that validated record before projection can see it.
    // Revalidating with serde first would only read every large payload again.
    extract_record(file, offset, length, true, digest)
}

fn extract_record(
    file: &mut File,
    offset: u64,
    length: u64,
    projection: bool,
    expected: &str,
) -> Result<Value> {
    file.seek(SeekFrom::Start(offset))?;
    let mut raw = LineReader::new(BufReader::new((&mut *file).take(length)));
    let record = {
        let mut buffered = BufReader::new(&mut raw);
        let record = stream::extract(&mut buffered, projection)?;
        std::io::copy(&mut buffered, &mut std::io::sink())?;
        record
    };
    ensure!(
        raw.count == length && format!("{:x}", raw.digest.finalize()) == expected,
        "Claude frozen record changed during read; reopen it"
    );
    Ok(record)
}

struct LineReader<R> {
    inner: R,
    digest: Sha256,
    count: u64,
    complete: bool,
    hashed_remaining: usize,
}
impl<R> LineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            count: 0,
            complete: false,
            hashed_remaining: 0,
        }
    }
}
impl<R: BufRead> Read for LineReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.complete || output.is_empty() {
            return Ok(0);
        }
        let input = self.inner.fill_buf()?;
        if self.hashed_remaining == 0 {
            self.hashed_remaining = input
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(input.len(), |index| index + 1);
            self.digest.update(&input[..self.hashed_remaining]);
        }
        let n = self.hashed_remaining.min(output.len());
        output[..n].copy_from_slice(&input[..n]);
        self.complete = n > 0 && output[n - 1] == b'\n';
        self.hashed_remaining -= n;
        self.count += n as u64;
        self.inner.consume(n);
        Ok(n)
    }
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
    fn validation_scratch_budget_is_not_a_record_limit() {
        for length in [1024, VALIDATION_SCRATCH_BYTES + 1024] {
            let mut file = tempfile::tempfile().unwrap();
            writeln!(file, "{}", json!({"ignored":"x".repeat(length)})).unwrap();
            let size = file.metadata().unwrap().len();
            let expected = source_digest(&mut file, size).unwrap();
            let record = validate_record(&mut file, 0, size).unwrap();
            assert_eq!(record.length, size);
            assert!(record.complete);
            assert!(record.validation.is_ok());
            assert_eq!(record.hash, expected);
        }
    }

    #[test]
    fn giant_hidden_blocks_are_streamed_and_visible_text_survives() {
        let mut file = tempfile::tempfile().unwrap();
        let record = json!({"type":"assistant","uuid":"one","parentUuid":null,"sessionId":ID,"message":{"role":"assistant","content":[{"type":"thinking","thinking":"x".repeat(2 * 1024 * 1024)},{"type":"image","source":{"data":"y".repeat(2 * 1024 * 1024)}},{"type":"text","text":"visible ☃\nline"}]}});
        writeln!(file, "{record}").unwrap();
        let size = file.metadata().unwrap().len();
        let (records, _) = scan(&mut file, size, ID).unwrap();
        assert!(retained_charge(&records[0]).unwrap() < 8192);
        let projected = read_record(&mut file, &records[0]).unwrap();
        assert_eq!(
            projected["message"]["content"][2]["text"],
            "visible ☃\nline"
        );
        assert!(serde_json::to_vec(&projected).unwrap().len() < 8192);
    }

    #[test]
    fn giant_native_result_is_not_mistaken_for_verified_channel_text() {
        let mut file = tempfile::tempfile().unwrap();
        writeln!(file, "{}", json!({"type":"user","uuid":"one","parentUuid":null,"sessionId":ID,"message":{"role":"user","content":[{"content":"x".repeat(2 * 1024 * 1024),"tool_use_id":"native-fetch","type":"tool_result"}]}})).unwrap();
        let size = file.metadata().unwrap().len();
        let (records, _) = scan(&mut file, size, ID).unwrap();
        let projected = read_record(&mut file, &records[0]).unwrap();
        assert_eq!(
            projected["message"]["content"][0]["tool_use_id"],
            "native-fetch"
        );
        assert!(super::super::text_parts(&projected["message"]["content"][0]["content"]).is_none());
        assert!(serde_json::to_vec(&projected).unwrap().len() < 8192);
    }

    #[test]
    fn malformed_giant_ignored_payload_still_fails_validation() {
        let mut file = tempfile::tempfile().unwrap();
        write!(file, "{{\"sessionId\":\"{ID}\",\"ignored\":\"").unwrap();
        file.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap();
        file.write_all(b"\\q\"}\n").unwrap();
        let size = file.metadata().unwrap().len();
        assert!(
            scan(&mut file, size, ID)
                .unwrap_err()
                .to_string()
                .contains("Malformed durable")
        );
    }

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
