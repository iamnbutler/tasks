//! Message types for LLM conversations.
//!
//! These types are provider-agnostic and map to/from provider-specific formats.

use serde::{Deserialize, Serialize};

/// Role of a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Content within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    Thinking { thinking: String },
    Image { media_type: String, data: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text { text: text.into() }
    }

    pub fn thinking(thinking: impl Into<String>) -> Self {
        Content::Thinking { thinking: thinking.into() }
    }

    pub fn image(media_type: impl Into<String>, base64_data: impl Into<String>) -> Self {
        Content::Image {
            media_type: media_type.into(),
            data: base64_data.into(),
        }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Content::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn as_thinking(&self) -> Option<&str> {
        match self {
            Content::Thinking { thinking } => Some(thinking),
            _ => None,
        }
    }

    /// Rough token estimate (chars / 4). Good enough for context window budgeting.
    pub fn estimate_tokens(&self) -> u32 {
        let chars = match self {
            Content::Text { text } => text.len(),
            Content::Thinking { thinking } => thinking.len(),
            Content::Image { data, .. } => data.len() / 3, // base64 → ~tokens
            Content::ToolUse { name, input, .. } => {
                name.len() + input.to_string().len()
            }
            Content::ToolResult { content, .. } => content.len(),
        };
        (chars as u32 / 4).max(1)
    }
}

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    pub fn new(role: Role, content: Vec<Content>) -> Self {
        Self { role, content }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, content: vec![Content::text(text)] }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![Content::text(text)] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: vec![Content::text(text)] }
    }

    /// Get the text content of this message, concatenating all text blocks.
    pub fn text(&self) -> String {
        self.content.iter().filter_map(|c| c.as_text()).collect::<Vec<_>>().join("")
    }

    /// Rough token estimate for this message (sum of content block estimates + overhead).
    pub fn estimate_tokens(&self) -> u32 {
        // ~4 tokens overhead per message for role/formatting
        4 + self.content.iter().map(|c| c.estimate_tokens()).sum::<u32>()
    }
}

/// Tool definition for function calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// Whether this tool is safe to execute concurrently with other safe tools.
    ///
    /// Read-only tools (file reads, searches, etc.) can be marked as concurrency-safe
    /// so they run in parallel, while mutating tools (file writes, shell commands)
    /// default to `false` and run serially.
    #[serde(default)]
    pub is_concurrency_safe: bool,
    /// Maximum result size in bytes before the output is persisted to disk.
    ///
    /// - `Some(n)`: persist to disk and return a preview if result exceeds `n` bytes
    /// - `None`: never persist (tool results are always returned in full)
    ///
    /// Defaults to `Some(100_000)` (100 KB). Set to `None` for tools like `Read`
    /// that already self-limit or return structured data the model must process.
    #[serde(default = "Tool::default_max_result_size")]
    pub max_result_size: Option<usize>,
}

/// Default max result size: 100 KB.
pub const DEFAULT_MAX_RESULT_SIZE: usize = 100_000;

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            is_concurrency_safe: false,
            max_result_size: Some(DEFAULT_MAX_RESULT_SIZE),
        }
    }

    /// Create a tool marked as safe for concurrent execution.
    pub fn new_concurrent(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            is_concurrency_safe: true,
            max_result_size: Some(DEFAULT_MAX_RESULT_SIZE),
        }
    }

    /// Set the max result size for this tool. `None` disables persistence.
    pub fn with_max_result_size(mut self, max_result_size: Option<usize>) -> Self {
        self.max_result_size = max_result_size;
        self
    }

    fn default_max_result_size() -> Option<usize> {
        Some(DEFAULT_MAX_RESULT_SIZE)
    }
}

/// A tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
    /// Supplementary context appended after the tool result in the same user message.
    ///
    /// Each note becomes an additional text content block following the tool_result block,
    /// so the model sees it as context right after the tool output. Useful for warnings,
    /// suggestions, or hints that shouldn't clutter the primary `content` field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ToolResult {
    pub fn success(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
            is_error: false,
            notes: Vec::new(),
        }
    }

    pub fn error(tool_call_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            content: error.into(),
            is_error: true,
            notes: Vec::new(),
        }
    }

    /// Append a supplementary note that the model will see after the tool result.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// Reason the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
}

/// Response from the LLM provider.
#[derive(Debug, Clone)]
pub struct Response {
    pub content: Vec<Content>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: Option<StopReason>,
    pub usage: Option<Usage>,
}

impl Response {
    pub fn text(&self) -> String {
        self.content.iter().filter_map(|c| c.as_text()).collect::<Vec<_>>().join("")
    }

    pub fn thinking(&self) -> String {
        self.content.iter().filter_map(|c| c.as_thinking()).collect::<Vec<_>>().join("")
    }
}

/// Token usage information.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl Usage {
    pub fn total(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_text() {
        let content = Content::text("hello");
        assert_eq!(content.as_text(), Some("hello"));
    }

    #[test]
    fn test_content_thinking() {
        let content = Content::thinking("reasoning");
        assert_eq!(content.as_thinking(), Some("reasoning"));
    }

    #[test]
    fn test_message_text_concatenation() {
        let msg = Message::new(Role::User, vec![
            Content::text("hello "),
            Content::text("world"),
        ]);
        assert_eq!(msg.text(), "hello world");
    }

    #[test]
    fn test_response_text_excludes_thinking() {
        let response = Response {
            content: vec![
                Content::thinking("let me reason"),
                Content::text("final answer"),
            ],
            tool_calls: vec![],
            stop_reason: Some(StopReason::EndTurn),
            usage: None,
        };
        assert_eq!(response.text(), "final answer");
        assert_eq!(response.thinking(), "let me reason");
    }

    #[test]
    fn test_tool_result_success() {
        let result = ToolResult::success("id-1", "output");
        assert!(!result.is_error);
        assert_eq!(result.content, "output");
    }

    #[test]
    fn test_tool_result_error() {
        let result = ToolResult::error("id-1", "failed");
        assert!(result.is_error);
        assert_eq!(result.content, "failed");
    }

    #[test]
    fn test_usage_total() {
        let usage = Usage { input_tokens: 100, output_tokens: 50 };
        assert_eq!(usage.total(), 150);
    }
}
