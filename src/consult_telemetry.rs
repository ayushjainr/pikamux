//! Bounded, per-turn consultation output and provider-reported measurements.
//! No transcripts, credentials, or provider reasoning text are retained here.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Instant;

pub const MAX_STREAM_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AnswerDelta {
    pub turn: u64,
    pub item_id: String,
    pub text: String,
}

impl AnswerDelta {
    pub fn valid(&self, turn: u64) -> bool {
        self.turn == turn
            && !self.item_id.is_empty()
            && self.item_id.len() <= 256
            && !self.text.is_empty()
            && self.text.len() <= MAX_STREAM_BYTES
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

impl TurnUsage {
    pub fn valid(&self) -> bool {
        self.cached_input_tokens <= self.input_tokens
            && self.reasoning_output_tokens <= self.output_tokens
    }

    pub(crate) fn from_codex(value: &Value) -> Option<Self> {
        let usage = Self {
            input_tokens: value.get("inputTokens")?.as_u64()?,
            cached_input_tokens: value.get("cachedInputTokens")?.as_u64()?,
            output_tokens: value.get("outputTokens")?.as_u64()?,
            reasoning_output_tokens: value.get("reasoningOutputTokens")?.as_u64()?,
        };
        usage.valid().then_some(usage)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TurnMetrics {
    pub provider_ack_seconds: Option<f64>,
    pub first_text_seconds: Option<f64>,
    pub completed_seconds: Option<f64>,
    pub usage: Option<TurnUsage>,
}

impl TurnMetrics {
    pub fn valid(&self) -> bool {
        [
            self.provider_ack_seconds,
            self.first_text_seconds,
            self.completed_seconds,
        ]
        .into_iter()
        .flatten()
        .all(|n| n.is_finite() && n >= 0.0)
            && self.usage.as_ref().is_none_or(TurnUsage::valid)
            && self
                .provider_ack_seconds
                .zip(self.first_text_seconds)
                .is_none_or(|(a, b)| a <= b)
            && self
                .first_text_seconds
                .zip(self.completed_seconds)
                .is_none_or(|(a, b)| a <= b)
    }
}

pub(crate) fn seconds(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_usage_is_not_zero_and_invalid_counts_are_rejected() {
        assert!(TurnUsage::from_codex(&serde_json::json!({})).is_none());
        assert!(
            TurnUsage::from_codex(&serde_json::json!({
                "inputTokens":1,"cachedInputTokens":2,"outputTokens":3,"reasoningOutputTokens":1
            }))
            .is_none()
        );
        assert!(
            !TurnMetrics {
                first_text_seconds: Some(f64::NAN),
                ..TurnMetrics::default()
            }
            .valid()
        );
    }
    #[test]
    fn stream_is_turn_bound_and_bounded() {
        let mut delta = AnswerDelta {
            turn: 1,
            item_id: "a".into(),
            text: "partial".into(),
        };
        assert!(delta.valid(1));
        assert!(!delta.valid(2));
        delta.text = "x".repeat(MAX_STREAM_BYTES + 1);
        assert!(!delta.valid(1));
    }
}
