//! Selective JSON extraction after independent validation of the original bytes.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::io::BufRead;

const TEXT_BYTES: usize = super::super::CLAUDE_PAGE_BYTES;
const METADATA_BYTES: usize = 32 * 1024 * 1024;

pub(super) fn extract(reader: impl BufRead, projection: bool) -> Result<Value> {
    let mut parser = Parser {
        reader,
        retained: 0,
        projection,
        depth: 0,
    };
    parser.value(&[], true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn escape_amplification_uses_decoded_text_budget() {
        let escaped = "\\u0061".repeat(2 * 1024 * 1024);
        let input = format!(
            "{{\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{escaped}\\ud83d\\ude00\"}}]}}}}"
        );
        let record = extract(Cursor::new(input), true).unwrap();
        let text = record["message"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text.len(), 2 * 1024 * 1024 + 4);
        assert!(text.ends_with('😀'));
    }

    #[test]
    fn nested_oversized_result_is_unverified_not_visible_text_failure() {
        let input = serde_json::json!({"message":{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","text":"x".repeat(128 * 1024)},{"type":"text","text":"hello"}],"tool_use_id":"fetch"}]}}).to_string();
        let record = extract(Cursor::new(input), true).unwrap();
        assert_eq!(record["message"]["content"][0]["tool_use_id"], "fetch");
        assert!(record["message"]["content"][0]["content"].is_object());
        assert!(
            super::super::super::text_parts(&record["message"]["content"][0]["content"]).is_none()
        );
    }

    #[test]
    fn skipped_string_still_validates_utf8_across_buffer_boundaries() {
        let mut input = b"{\"ignored\":\"valid ".to_vec();
        input.extend_from_slice("☃😀".as_bytes());
        input.extend_from_slice(b"\"}");
        assert!(
            extract(
                std::io::BufReader::with_capacity(2, Cursor::new(input)),
                false
            )
            .is_ok()
        );
        let invalid = b"{\"ignored\":\"bad \xff\"}";
        let error = extract(Cursor::new(invalid), false).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }

    #[test]
    fn oversized_string_preserves_incomplete_utf8_at_skip_boundary() {
        let mut parser = Parser {
            reader: std::io::BufReader::with_capacity(2, Cursor::new("\"☃😀\"".as_bytes())),
            retained: 0,
            projection: true,
            depth: 0,
        };
        assert!(parser.string(true, 1).unwrap().is_none());
        assert!(parser.peek().unwrap().is_none());
    }

    #[test]
    fn native_text_limit_counts_text_not_json_container_overhead() {
        for pieces in [
            vec!["x".repeat(64 * 1024)],
            vec!["x".repeat(32 * 1024), "y".repeat(32 * 1024)],
        ] {
            let expected = pieces.concat();
            let parts: Vec<Value> = pieces
                .into_iter()
                .map(|text| serde_json::json!({"type":"text","text":text}))
                .collect();
            let input = serde_json::json!({"message":{"role":"user","content":[{"type":"tool_result","content":parts,"tool_use_id":"fetch"}]}}).to_string();
            let record = extract(Cursor::new(input), true).unwrap();
            assert_eq!(
                super::super::super::text_parts(&record["message"]["content"][0]["content"]),
                Some(expected)
            );
        }
    }

    #[test]
    fn repeated_identity_field_is_refused() {
        let error = extract(
            Cursor::new(br#"{"sessionId":"one","sessionId":"two"}"#),
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Repeated Claude retained JSON key")
        );
    }
}

struct Parser<R> {
    reader: R,
    retained: usize,
    projection: bool,
    depth: usize,
}

impl<R: BufRead> Parser<R> {
    fn byte(&mut self) -> Result<Option<u8>> {
        let mut byte = [0];
        Ok((self.reader.read(&mut byte)? != 0).then_some(byte[0]))
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        loop {
            let buffer = self.reader.fill_buf()?;
            let Some(&byte) = buffer.first() else {
                return Ok(None);
            };
            if byte.is_ascii_whitespace() {
                self.reader.consume(1);
            } else {
                return Ok(Some(byte));
            }
        }
    }

    fn string(&mut self, retain: bool, limit: usize) -> Result<Option<String>> {
        ensure!(self.byte()? == Some(b'"'), "Invalid Claude JSON string");
        if !retain {
            self.skip_string_tail()?;
            return Ok(None);
        }
        let Some(bytes) = self.string_bytes(limit)? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes).context("Invalid Claude JSON UTF-8 string")?;
        self.retained = self
            .retained
            .checked_add(text.len())
            .context("Claude retained text overflow")?;
        ensure!(
            self.retained <= METADATA_BYTES,
            "Claude record retained metadata exceeds bounded memory budget"
        );
        Ok(Some(text))
    }

    fn string_bytes(&mut self, limit: usize) -> Result<Option<Vec<u8>>> {
        let mut bytes = Vec::new();
        loop {
            let buffer = self.reader.fill_buf()?;
            let plain = buffer
                .iter()
                .position(|byte| matches!(byte, b'"' | b'\\'))
                .unwrap_or(buffer.len());
            if plain > 0 {
                if plain > limit - bytes.len() {
                    self.skip_string_with_prefix(&bytes)?;
                    return Ok(None);
                }
                bytes.extend_from_slice(&buffer[..plain]);
                self.reader.consume(plain);
                continue;
            }
            let byte = self.byte()?.context("Incomplete Claude JSON string")?;
            if byte == b'"' {
                break;
            }
            let mut utf8 = [0; 4];
            let decoded: &[u8] = if byte == b'\\' {
                self.decode_escape(&mut utf8)?
            } else {
                utf8[0] = byte;
                &utf8[..1]
            };
            if bytes.len() + decoded.len() > limit {
                self.skip_string_tail()?;
                return Ok(None);
            } else {
                bytes.extend_from_slice(decoded);
            }
        }
        Ok(Some(bytes))
    }

    fn decode_escape<'a>(&mut self, utf8: &'a mut [u8; 4]) -> Result<&'a [u8]> {
        let escape = self.byte()?.context("Incomplete Claude JSON escape")?;
        Ok(match escape {
            b'"' => b"\"",
            b'\\' => b"\\",
            b'/' => b"/",
            b'b' => b"\x08",
            b'f' => b"\x0c",
            b'n' => b"\n",
            b'r' => b"\r",
            b't' => b"\t",
            b'u' => {
                let scalar = self.unicode_scalar()?;
                char::from_u32(scalar)
                    .context("Invalid Claude JSON unicode scalar")?
                    .encode_utf8(utf8)
                    .as_bytes()
            }
            _ => bail!("Invalid Claude JSON escape"),
        })
    }

    fn unicode_scalar(&mut self) -> Result<u32> {
        let first = self.hex_quad()?;
        if !(0xd800..=0xdbff).contains(&first) {
            return Ok(u32::from(first));
        }
        ensure!(
            self.byte()? == Some(b'\\') && self.byte()? == Some(b'u'),
            "Invalid Claude JSON surrogate"
        );
        let second = self.hex_quad()?;
        ensure!(
            (0xdc00..=0xdfff).contains(&second),
            "Invalid Claude JSON surrogate"
        );
        Ok(0x10000 + ((u32::from(first) - 0xd800) << 10) + u32::from(second) - 0xdc00)
    }

    fn skip_string_tail(&mut self) -> Result<()> {
        self.skip_string_with_prefix(&[])
    }

    fn skip_string_with_prefix(&mut self, prefix: &[u8]) -> Result<()> {
        let mut utf8 = Utf8Tail::default();
        utf8.feed(prefix)?;
        loop {
            let buffer = self.reader.fill_buf()?;
            ensure!(!buffer.is_empty(), "Incomplete Claude JSON string");
            let n = buffer
                .iter()
                .position(|byte| matches!(byte, b'"' | b'\\'))
                .unwrap_or(buffer.len());
            utf8.feed(&buffer[..n])?;
            self.reader.consume(n);
            let buffer = self.reader.fill_buf()?;
            let Some(&byte) = buffer.first() else {
                continue;
            };
            if !matches!(byte, b'"' | b'\\') {
                continue;
            }
            self.reader.consume(1);
            ensure!(utf8.pending.is_empty(), "Invalid Claude JSON UTF-8 string");
            if byte == b'"' {
                return Ok(());
            }
            self.decode_escape(&mut [0; 4])?;
        }
    }

    fn hex_quad(&mut self) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self
                .byte()?
                .context("Incomplete Claude JSON unicode escape")?;
            let digit = char::from(byte)
                .to_digit(16)
                .context("Invalid Claude JSON unicode escape")?;
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }

    fn keep(&self, path: &[String], key: &str) -> bool {
        match path.first().map(String::as_str) {
            None => {
                matches!(
                    key,
                    "type"
                        | "uuid"
                        | "parentUuid"
                        | "sessionId"
                        | "isSidechain"
                        | "isMeta"
                        | "subtype"
                        | "compactMetadata"
                ) || self.projection && key == "message"
            }
            Some("compactMetadata") => true,
            Some("message") => match path.len() {
                1 => matches!(key, "role" | "content"),
                _ => matches!(
                    key,
                    "type"
                        | "text"
                        | "content"
                        | "id"
                        | "tool_use_id"
                        | "is_error"
                        | "name"
                        | "input"
                        | "operation_id"
                ),
            },
            _ => false,
        }
    }

    fn value(&mut self, path: &[String], retain: bool) -> Result<Value> {
        ensure!(self.depth < 128, "Claude JSON recursion limit exceeded");
        self.depth += 1;
        let value = self.value_inner(path, retain);
        self.depth -= 1;
        value
    }

    fn value_inner(&mut self, path: &[String], retain: bool) -> Result<Value> {
        match self.peek()?.context("Incomplete Claude JSON value")? {
            b'"' => self.string_value(path, retain),
            b'{' => self.object_value(path, retain),
            b'[' => self.array_value(path, retain),
            _ => self.scalar_value(retain),
        }
    }

    fn string_value(&mut self, path: &[String], retain: bool) -> Result<Value> {
        let message = path.first().is_some_and(|key| key == "message");
        let limit = if message
            && (path.len() > 3
                || path.len() == 3 && path.last().is_some_and(|key| key == "content"))
        {
            64 * 1024
        } else if message {
            TEXT_BYTES
        } else {
            METADATA_BYTES
        };
        match self.string(retain, limit)? {
            Some(text) => Ok(Value::String(text)),
            None if retain && !message => {
                bail!("Claude ancestry metadata exceeds its bounded memory budget")
            }
            None if retain => Ok(serde_json::json!({"_pikaOversizedText": true})),
            None => Ok(Value::Null),
        }
    }

    fn object_value(&mut self, path: &[String], retain: bool) -> Result<Value> {
        self.byte()?;
        let mut object = Map::new();
        if self.peek()? == Some(b'}') {
            self.byte()?;
            return Ok(Value::Object(object));
        }
        loop {
            self.object_field(path, retain, &mut object)?;
            if self.container_end(b'}')? {
                break;
            }
        }
        self.validate_visible(path, &object)?;
        Ok(Value::Object(object))
    }

    fn object_field(
        &mut self,
        path: &[String],
        retain: bool,
        object: &mut Map<String, Value>,
    ) -> Result<()> {
        self.peek()?;
        let before_key = self.retained;
        // Keys remain bounded even in ignored subtrees. Validation has
        // already established JSON grammar independently.
        let key = self.object_key(retain)?;
        ensure!(self.peek()? == Some(b':'), "Invalid Claude JSON object");
        self.byte()?;
        let keep = retain && key.as_ref().is_some_and(|key| self.keep(path, key));
        let mut child = path.to_vec();
        if keep {
            child.push(key.clone().unwrap());
        }
        let value = self.value(&child, keep)?;
        if keep {
            self.insert_field(object, key.unwrap(), value)?;
        } else {
            self.retained = before_key;
        }
        Ok(())
    }

    fn insert_field(
        &mut self,
        object: &mut Map<String, Value>,
        key: String,
        value: Value,
    ) -> Result<()> {
        ensure!(
            !object.contains_key(&key),
            "Repeated Claude retained JSON key"
        );
        self.retained = self
            .retained
            .checked_add(256 + std::mem::size_of::<Value>())
            .context("Claude retained metadata overflow")?;
        ensure!(
            self.retained <= METADATA_BYTES,
            "Claude record retained metadata exceeds bounded memory budget"
        );
        object.insert(key, value);
        Ok(())
    }

    fn object_key(&mut self, retain: bool) -> Result<Option<String>> {
        let key = self.string(retain, 4096)?;
        ensure!(
            !retain || key.is_some(),
            "Claude JSON key exceeds metadata bound"
        );
        Ok(key)
    }

    fn container_end(&mut self, close: u8) -> Result<bool> {
        let next = self.peek()?.context("Incomplete Claude JSON container")?;
        ensure!(
            next == b',' || next == close,
            "Invalid Claude JSON container"
        );
        self.byte()?;
        Ok(next == close)
    }

    fn validate_visible(&self, path: &[String], object: &Map<String, Value>) -> Result<()> {
        if path.first().is_some_and(|key| key == "message") {
            if path.len() == 2 && object.get("type").and_then(Value::as_str) == Some("text") {
                ensure!(
                    !object.get("text").is_some_and(|value| value.is_object()),
                    "Claude visible message exceeds the 8 MiB mobile history item limit"
                );
            }
            if path.len() == 1 {
                ensure!(
                    !object.get("content").is_some_and(Value::is_object),
                    "Claude visible message exceeds the 8 MiB mobile history item limit"
                );
            }
        }
        Ok(())
    }

    fn array_value(&mut self, path: &[String], retain: bool) -> Result<Value> {
        self.byte()?;
        let mut values = Vec::new();
        let before = self.retained;
        let bounded_result = path.len() >= 3 && path.get(2).is_some_and(|key| key == "content");
        let mut oversized = false;
        let mut text_bytes = 0usize;
        if self.peek()? == Some(b']') {
            self.byte()?;
            return Ok(Value::Array(values));
        }
        loop {
            let value = self.value(path, retain && !oversized)?;
            if retain && !oversized {
                oversized = self.retain_array_value(
                    &mut values,
                    value,
                    before,
                    bounded_result,
                    &mut text_bytes,
                )?;
            }
            if self.container_end(b']')? {
                break;
            }
        }
        if oversized {
            Ok(serde_json::json!({"_pikaOversizedText": true}))
        } else {
            Ok(Value::Array(values))
        }
    }

    fn retain_array_value(
        &mut self,
        values: &mut Vec<Value>,
        value: Value,
        before: usize,
        bounded_result: bool,
        text_bytes: &mut usize,
    ) -> Result<bool> {
        self.retained += std::mem::size_of::<Value>();
        ensure!(
            self.retained <= METADATA_BYTES,
            "Claude record retained metadata exceeds bounded memory budget"
        );
        if value["type"] == "text" {
            *text_bytes += value["text"].as_str().map_or(0, str::len);
        }
        let oversized = bounded_result && (oversized_text(&value) || *text_bytes > 64 * 1024);
        if oversized {
            values.clear();
            self.retained = before;
        } else {
            values.push(value);
        }
        Ok(oversized)
    }

    fn scalar_value(&mut self, retain: bool) -> Result<Value> {
        let mut bytes = Vec::new();
        loop {
            let buffer = self.reader.fill_buf()?;
            let Some(&byte) = buffer.first() else { break };
            if byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}') {
                break;
            }
            self.reader.consume(1);
            if retain {
                ensure!(
                    bytes.len() < 4096,
                    "Claude JSON scalar exceeds metadata bound"
                );
                bytes.push(byte);
            }
        }
        if retain {
            Ok(serde_json::from_slice(&bytes)?)
        } else {
            Ok(Value::Null)
        }
    }
}

fn oversized_text(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key("_pikaOversizedText") || object.values().any(oversized_text)
        }
        Value::Array(array) => array.iter().any(oversized_text),
        _ => false,
    }
}

#[derive(Default)]
struct Utf8Tail {
    pending: Vec<u8>,
}

impl Utf8Tail {
    fn feed(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !self.pending.is_empty() && !bytes.is_empty() {
            self.pending.push(bytes[0]);
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.pending) {
                Ok(_) => self.pending.clear(),
                Err(error) => ensure!(
                    error.error_len().is_none(),
                    "Invalid Claude JSON UTF-8 string"
                ),
            }
        }
        if self.pending.is_empty() {
            if let Err(error) = std::str::from_utf8(bytes) {
                ensure!(
                    error.error_len().is_none(),
                    "Invalid Claude JSON UTF-8 string"
                );
                self.pending
                    .extend_from_slice(&bytes[error.valid_up_to()..]);
            }
        }
        ensure!(self.pending.len() <= 3, "Invalid Claude JSON UTF-8 string");
        Ok(())
    }
}
