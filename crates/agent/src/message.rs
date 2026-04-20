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
    /// Whether this tool has no side effects (no filesystem/process/network writes).
    ///
    /// Read-only tools can freely parallelize and are safe to speculatively execute.
    #[serde(default)]
    pub is_read_only: bool,
    /// Whether this tool performs hard-to-reverse operations (delete, overwrite, drop).
    ///
    /// Destructive tools warrant extra scrutiny: higher confirmation thresholds,
    /// serial execution, audit logging, etc.
    #[serde(default)]
    pub is_destructive: bool,
    /// Name of the input parameter (if any) that carries a filesystem path.
    ///
    /// Enables path-aware behavior (per-path locking, permission checks, surfacing
    /// which file a tool call targets) without hard-coding per-tool logic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_parameter: Option<String>,
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
            is_read_only: false,
            is_destructive: false,
            path_parameter: None,
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
            is_read_only: false,
            is_destructive: false,
            path_parameter: None,
            max_result_size: Some(DEFAULT_MAX_RESULT_SIZE),
        }
    }

    /// Set the max result size for this tool. `None` disables persistence.
    pub fn with_max_result_size(mut self, max_result_size: Option<usize>) -> Self {
        self.max_result_size = max_result_size;
        self
    }

    /// Mark this tool as read-only (no side effects).
    pub fn read_only(mut self) -> Self {
        self.is_read_only = true;
        self
    }

    /// Mark this tool as destructive (hard to reverse).
    pub fn destructive(mut self) -> Self {
        self.is_destructive = true;
        self
    }

    /// Declare the input parameter that carries a filesystem path.
    pub fn with_path_parameter(mut self, parameter: impl Into<String>) -> Self {
        self.path_parameter = Some(parameter.into());
        self
    }

    /// Extract the path this tool call targets, if any.
    ///
    /// Returns `Some(path)` when `path_parameter` is set and the input object
    /// contains that key with a string value.
    pub fn get_path<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        self.path_parameter
            .as_deref()
            .and_then(|param| input.get(param))
            .and_then(|v| v.as_str())
    }

    /// Whether this tool can run in parallel with other safe tools.
    ///
    /// True if explicitly marked concurrency-safe, or if it's read-only and not destructive.
    pub fn can_parallelize(&self) -> bool {
        self.is_concurrency_safe || (self.is_read_only && !self.is_destructive)
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

    #[test]
    fn test_tool_defaults_are_safe() {
        let tool = Tool::new("write_file", "writes", serde_json::json!({}));
        assert!(!tool.is_concurrency_safe);
        assert!(!tool.is_read_only);
        assert!(!tool.is_destructive);
        assert!(tool.path_parameter.is_none());
        assert!(!tool.can_parallelize());
    }

    #[test]
    fn test_read_only_tool_can_parallelize() {
        let tool = Tool::new("read_file", "reads", serde_json::json!({})).read_only();
        assert!(tool.is_read_only);
        assert!(!tool.is_destructive);
        assert!(tool.can_parallelize());
    }

    #[test]
    fn test_destructive_read_only_is_not_parallelizable() {
        // Contrived, but codifies the rule: destructive overrides read_only for parallelization.
        let tool = Tool::new("purge", "wipes", serde_json::json!({}))
            .read_only()
            .destructive();
        assert!(!tool.can_parallelize());
    }

    #[test]
    fn test_concurrency_safe_overrides_other_flags() {
        let tool = Tool::new_concurrent("search", "searches", serde_json::json!({}));
        assert!(tool.can_parallelize());
    }

    #[test]
    fn test_get_path_extracts_declared_parameter() {
        let tool =
            Tool::new("read_file", "reads", serde_json::json!({})).with_path_parameter("file_path");
        let input = serde_json::json!({ "file_path": "/tmp/foo.rs", "offset": 0 });
        assert_eq!(tool.get_path(&input), Some("/tmp/foo.rs"));
    }

    #[test]
    fn test_get_path_returns_none_when_parameter_missing() {
        let tool = Tool::new("read_file", "reads", serde_json::json!({}))
            .with_path_parameter("file_path");
        let input = serde_json::json!({ "other": "value" });
        assert_eq!(tool.get_path(&input), None);
    }

    #[test]
    fn test_get_path_returns_none_when_not_declared() {
        let tool = Tool::new("no_path", "generic", serde_json::json!({}));
        let input = serde_json::json!({ "file_path": "/tmp/foo.rs" });
        assert_eq!(tool.get_path(&input), None);
    }

    #[test]
    fn test_tool_new_metadata_fields_deserialize_defaults() {
        // Existing callers serializing Tool without new fields should deserialize safely.
        let json = serde_json::json!({
            "name": "legacy",
            "description": "",
            "parameters": {},
        });
        let tool: Tool = serde_json::from_value(json).unwrap();
        assert!(!tool.is_read_only);
        assert!(!tool.is_destructive);
        assert!(tool.path_parameter.is_none());
    }
}
