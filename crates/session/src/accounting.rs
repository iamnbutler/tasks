//! Token and cost accounting — spec §16.4.
//!
//! Parses agent output to extract token usage information. Claude Code and other
//! agents may emit token counts in various formats. This module handles the
//! extraction leniently, supporting multiple payload shapes.

use serde::{Deserialize, Serialize};

/// Token usage extracted from agent output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn new(input: u64, output: u64) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
        }
    }

    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Check if this represents any token usage.
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

impl std::ops::Add for TokenUsage {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
        }
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
    }
}

/// Tracks cumulative token usage for a session.
///
/// Per spec §13.5, we prefer absolute totals when available and compute deltas
/// to avoid double-counting.
#[derive(Debug, Default)]
pub struct TokenTracker {
    /// Last reported cumulative totals from the agent.
    last_reported: TokenUsage,
    /// Accumulated tokens from deltas.
    accumulated: TokenUsage,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a token usage report from agent output.
    ///
    /// If `is_cumulative` is true, the usage represents total tokens used so far
    /// and we compute the delta. Otherwise, we add the usage directly.
    pub fn record(&mut self, usage: TokenUsage, is_cumulative: bool) {
        if is_cumulative {
            // Compute delta from last reported totals
            let delta = TokenUsage {
                input_tokens: usage.input_tokens.saturating_sub(self.last_reported.input_tokens),
                output_tokens: usage.output_tokens.saturating_sub(self.last_reported.output_tokens),
            };
            self.accumulated += delta;
            self.last_reported = usage;
        } else {
            // Direct delta, add to accumulated
            self.accumulated += usage;
        }
    }

    /// Get the total accumulated token usage.
    pub fn total(&self) -> TokenUsage {
        self.accumulated
    }

    /// Get the last reported cumulative values (for session end accounting).
    pub fn last_reported(&self) -> TokenUsage {
        self.last_reported
    }
}

/// Parser for extracting token usage from agent output text.
///
/// Agent output may contain JSON-formatted token usage in various shapes:
/// - `{"type": "thread/tokenUsage/updated", "data": {"inputTokens": N, "outputTokens": M}}`
/// - `{"total_token_usage": {"input_tokens": N, "output_tokens": M}}`
/// - `{"usage": {"input_tokens": N, "output_tokens": M}}`
/// - Lines containing token counts in other formats
pub struct TokenParser;

impl TokenParser {
    /// Try to extract token usage from a line of agent output.
    ///
    /// Returns `Some((usage, is_cumulative))` if token usage was found.
    /// The `is_cumulative` flag indicates whether the values represent
    /// cumulative totals (preferred) or deltas.
    pub fn parse(text: &str) -> Option<(TokenUsage, bool)> {
        // Try to parse as JSON first
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            return Self::parse_json(&value);
        }

        // Try to find JSON embedded in the text
        if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                if start < end {
                    let json_str = &text[start..=end];
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                        return Self::parse_json(&value);
                    }
                }
            }
        }

        None
    }

    fn parse_json(value: &serde_json::Value) -> Option<(TokenUsage, bool)> {
        // Check for thread/tokenUsage/updated events (cumulative totals, preferred)
        if let Some(event_type) = value.get("type").and_then(|v| v.as_str()) {
            if event_type.contains("tokenUsage") || event_type.contains("token_usage") {
                if let Some(data) = value.get("data") {
                    if let Some(usage) = Self::extract_from_object(data) {
                        return Some((usage, true)); // cumulative
                    }
                }
                // Try extracting directly from the event
                if let Some(usage) = Self::extract_from_object(value) {
                    return Some((usage, true)); // cumulative
                }
            }
        }

        // Check for total_token_usage wrapper (cumulative totals, preferred)
        if let Some(total) = value.get("total_token_usage") {
            if let Some(usage) = Self::extract_from_object(total) {
                return Some((usage, true)); // cumulative
            }
        }

        // Check for totalTokenUsage (camelCase variant)
        if let Some(total) = value.get("totalTokenUsage") {
            if let Some(usage) = Self::extract_from_object(total) {
                return Some((usage, true)); // cumulative
            }
        }

        // Check for usage object (may be delta or cumulative depending on context)
        // Per spec §13.5, don't treat generic `usage` as cumulative unless context defines it
        if let Some(usage_obj) = value.get("usage") {
            if let Some(usage) = Self::extract_from_object(usage_obj) {
                return Some((usage, false)); // treat as delta to be safe
            }
        }

        // Try extracting directly from the root object
        if let Some(usage) = Self::extract_from_object(value) {
            // Check if this looks like a cumulative total based on context
            let is_cumulative = value.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t.contains("total") || t.contains("cumulative"))
                .unwrap_or(false);
            return Some((usage, is_cumulative));
        }

        None
    }

    /// Extract token counts from a JSON object, handling various field names.
    fn extract_from_object(obj: &serde_json::Value) -> Option<TokenUsage> {
        let input = Self::extract_token_count(obj, &[
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
        ])?;

        let output = Self::extract_token_count(obj, &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
        ]).unwrap_or(0);

        if input == 0 && output == 0 {
            return None;
        }

        Some(TokenUsage::new(input, output))
    }

    fn extract_token_count(obj: &serde_json::Value, keys: &[&str]) -> Option<u64> {
        for key in keys {
            if let Some(count) = obj.get(*key).and_then(|v| v.as_u64()) {
                return Some(count);
            }
            // Try as i64 and convert
            if let Some(count) = obj.get(*key).and_then(|v| v.as_i64()) {
                if count >= 0 {
                    return Some(count as u64);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_total() {
        let usage = TokenUsage::new(100, 50);
        assert_eq!(usage.total(), 150);
    }

    #[test]
    fn token_usage_add() {
        let a = TokenUsage::new(100, 50);
        let b = TokenUsage::new(200, 100);
        let sum = a + b;
        assert_eq!(sum.input_tokens, 300);
        assert_eq!(sum.output_tokens, 150);
    }

    #[test]
    fn token_tracker_delta_mode() {
        let mut tracker = TokenTracker::new();
        tracker.record(TokenUsage::new(100, 50), false);
        tracker.record(TokenUsage::new(200, 100), false);
        assert_eq!(tracker.total().input_tokens, 300);
        assert_eq!(tracker.total().output_tokens, 150);
    }

    #[test]
    fn token_tracker_cumulative_mode() {
        let mut tracker = TokenTracker::new();
        // First report: 100 input, 50 output
        tracker.record(TokenUsage::new(100, 50), true);
        assert_eq!(tracker.total().input_tokens, 100);
        assert_eq!(tracker.total().output_tokens, 50);

        // Second report: 300 input, 150 output (cumulative)
        // Delta should be 200 input, 100 output
        tracker.record(TokenUsage::new(300, 150), true);
        assert_eq!(tracker.total().input_tokens, 300);
        assert_eq!(tracker.total().output_tokens, 150);
    }

    #[test]
    fn parse_thread_token_usage_event() {
        let json = r#"{"type": "thread/tokenUsage/updated", "data": {"inputTokens": 1500, "outputTokens": 800}}"#;
        let (usage, is_cumulative) = TokenParser::parse(json).unwrap();
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
        assert!(is_cumulative);
    }

    #[test]
    fn parse_total_token_usage() {
        let json = r#"{"total_token_usage": {"input_tokens": 2000, "output_tokens": 1000}}"#;
        let (usage, is_cumulative) = TokenParser::parse(json).unwrap();
        assert_eq!(usage.input_tokens, 2000);
        assert_eq!(usage.output_tokens, 1000);
        assert!(is_cumulative);
    }

    #[test]
    fn parse_usage_object() {
        let json = r#"{"usage": {"input_tokens": 500, "output_tokens": 250}}"#;
        let (usage, is_cumulative) = TokenParser::parse(json).unwrap();
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 250);
        assert!(!is_cumulative); // Generic usage treated as delta
    }

    #[test]
    fn parse_camel_case_variants() {
        let json = r#"{"totalTokenUsage": {"promptTokens": 1000, "completionTokens": 500}}"#;
        let (usage, is_cumulative) = TokenParser::parse(json).unwrap();
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert!(is_cumulative);
    }

    #[test]
    fn parse_embedded_json() {
        let text = "Agent output: {\"usage\": {\"input_tokens\": 100, \"output_tokens\": 50}} more text";
        let (usage, _) = TokenParser::parse(text).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn parse_no_token_usage() {
        let text = "Just a normal agent message without token info";
        assert!(TokenParser::parse(text).is_none());
    }

    #[test]
    fn parse_invalid_json() {
        let text = "{not valid json";
        assert!(TokenParser::parse(text).is_none());
    }

    #[test]
    fn parse_json_without_tokens() {
        let json = r#"{"type": "message", "content": "Hello"}"#;
        assert!(TokenParser::parse(json).is_none());
    }

    #[test]
    fn token_usage_is_empty() {
        assert!(TokenUsage::default().is_empty());
        assert!(!TokenUsage::new(1, 0).is_empty());
        assert!(!TokenUsage::new(0, 1).is_empty());
    }
}
