//! Token usage parser for agent output (spec §13.5).
//!
//! Parses token usage information from agent stdout/stderr output.
//! Agents may emit token counts in various formats; this module extracts
//! them leniently.

use models::accounting::TokenUsage;
use serde_json::Value;

/// Attempt to parse token usage from a line of agent output.
///
/// Returns Some(TokenUsage) if the line contains recognizable token counts.
/// The parser handles multiple formats:
///
/// - Direct JSON: `{"input_tokens": N, "output_tokens": M}`
/// - Nested in usage: `{"usage": {"input_tokens": N, "output_tokens": M}}`
/// - Thread token usage: `{"type": "thread/tokenUsage/updated", "data": {...}}`
/// - Total token usage wrapper: `{"total_token_usage": {...}}`
///
/// Per spec §13.5, we prefer absolute totals and extract counts leniently
/// from common field names.
pub fn parse_token_usage(line: &str) -> Option<TokenUsage> {
    // Try to parse as JSON
    let value: Value = serde_json::from_str(line).ok()?;

    // Try various extraction strategies
    extract_from_value(&value)
}

/// Extract token usage from a JSON value, trying multiple locations.
fn extract_from_value(value: &Value) -> Option<TokenUsage> {
    // Strategy 1: Direct top-level input_tokens/output_tokens
    if let Some(usage) = extract_direct(value) {
        return Some(usage);
    }

    // Strategy 2: Nested in "usage" field
    if let Some(usage_obj) = value.get("usage") {
        if let Some(usage) = extract_direct(usage_obj) {
            return Some(usage);
        }
    }

    // Strategy 3: Thread token usage format
    // {"type": "thread/tokenUsage/updated", "data": {"input_tokens": N, ...}}
    if value.get("type").and_then(|t| t.as_str()) == Some("thread/tokenUsage/updated") {
        if let Some(data) = value.get("data") {
            if let Some(usage) = extract_direct(data) {
                return Some(usage);
            }
        }
    }

    // Strategy 4: Total token usage wrapper
    // {"total_token_usage": {"input_tokens": N, ...}}
    if let Some(total) = value.get("total_token_usage") {
        if let Some(usage) = extract_direct(total) {
            return Some(usage);
        }
    }

    // Strategy 5: Anthropic API format
    // {"usage": {"input_tokens": N, "output_tokens": M}}
    if let Some(usage) = value.get("usage").or(value.get("token_usage")) {
        if let Some(u) = extract_direct(usage) {
            return Some(u);
        }
    }

    // Strategy 6: Check for message/content with usage
    // Claude Code may wrap usage in a message structure
    if let Some(content) = value.get("content") {
        if let Some(u) = extract_from_value(content) {
            return Some(u);
        }
    }

    None
}

/// Extract token usage directly from an object with token fields.
fn extract_direct(value: &Value) -> Option<TokenUsage> {
    let obj = value.as_object()?;

    // Look for input tokens with various field names
    let input = extract_token_count(obj, &[
        "input_tokens",
        "inputTokens",
        "prompt_tokens",
        "promptTokens",
    ])?;

    // Look for output tokens with various field names
    let output = extract_token_count(obj, &[
        "output_tokens",
        "outputTokens",
        "completion_tokens",
        "completionTokens",
    ])?;

    // Extract model if present
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(TokenUsage {
        input_tokens: input,
        output_tokens: output,
        model,
    })
}

/// Extract a token count from an object, trying multiple field names.
fn extract_token_count(obj: &serde_json::Map<String, Value>, field_names: &[&str]) -> Option<u64> {
    for name in field_names {
        if let Some(value) = obj.get(*name) {
            if let Some(n) = value.as_u64() {
                return Some(n);
            }
            if let Some(n) = value.as_i64() {
                return Some(n.max(0) as u64);
            }
            if let Some(n) = value.as_f64() {
                return Some(n.max(0.0) as u64);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_format() {
        let line = r#"{"input_tokens": 1500, "output_tokens": 800}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
    }

    #[test]
    fn parse_with_model() {
        let line = r#"{"input_tokens": 1500, "output_tokens": 800, "model": "claude-3-opus"}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
        assert_eq!(usage.model, Some("claude-3-opus".to_string()));
    }

    #[test]
    fn parse_nested_usage() {
        let line = r#"{"usage": {"input_tokens": 1000, "output_tokens": 500}}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
    }

    #[test]
    fn parse_thread_token_usage() {
        let line = r#"{"type": "thread/tokenUsage/updated", "data": {"input_tokens": 2000, "output_tokens": 1000}}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 2000);
        assert_eq!(usage.output_tokens, 1000);
    }

    #[test]
    fn parse_total_token_usage() {
        let line = r#"{"total_token_usage": {"input_tokens": 5000, "output_tokens": 2500}}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 5000);
        assert_eq!(usage.output_tokens, 2500);
    }

    #[test]
    fn parse_camel_case() {
        let line = r#"{"inputTokens": 1500, "outputTokens": 800}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
    }

    #[test]
    fn parse_prompt_completion_format() {
        let line = r#"{"prompt_tokens": 1500, "completion_tokens": 800}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
    }

    #[test]
    fn parse_non_json_returns_none() {
        assert!(parse_token_usage("hello world").is_none());
    }

    #[test]
    fn parse_json_without_tokens_returns_none() {
        let line = r#"{"type": "message", "content": "hello"}"#;
        assert!(parse_token_usage(line).is_none());
    }

    #[test]
    fn parse_partial_tokens_returns_none() {
        // Only input tokens, no output
        let line = r#"{"input_tokens": 1500}"#;
        assert!(parse_token_usage(line).is_none());
    }

    #[test]
    fn parse_negative_tokens_coerced_to_zero() {
        let line = r#"{"input_tokens": -100, "output_tokens": 800}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 800);
    }

    #[test]
    fn parse_float_tokens() {
        let line = r#"{"input_tokens": 1500.5, "output_tokens": 800.7}"#;
        let usage = parse_token_usage(line).unwrap();
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
    }
}
