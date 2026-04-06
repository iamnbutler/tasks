//! Provider trait for LLM backends.
//!
//! Abstracts over different LLM APIs (Anthropic, etc.)
//! allowing the agent to be provider-agnostic.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::AgentError;
use crate::message::{Message, Response, Tool, ToolResult};

/// Configuration for a completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionConfig {
    /// Model identifier (e.g., "claude-sonnet-4-6")
    pub model: String,
    /// Maximum tokens to generate
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Context window size (input token limit). Messages are truncated to fit.
    /// When constructed via [`CompletionConfig::new`], inferred from model name
    /// (200k for Claude models, 128k otherwise). When deserialized without an
    /// explicit value, falls back to the conservative 128k default.
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    /// Temperature for sampling (0.0 - 1.0)
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Top-p sampling parameter
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Stop sequences
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    /// Fraction of context window that triggers compaction (0.0–1.0).
    /// When estimated tokens exceed `context_window * compact_threshold`,
    /// the session summarizes older messages instead of dropping them.
    /// Defaults to 0.85 (85%).
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold: f32,
}

fn default_compact_threshold() -> f32 {
    0.85
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_context_window() -> u32 {
    // Conservative default for deserialization (non-Claude models).
    // CompletionConfig::new() overrides this via context_window_for_model().
    128_000
}

/// Infer context window size from model name.
fn context_window_for_model(model: &str) -> u32 {
    // All current Claude models support 200k context
    if model.contains("claude") {
        200_000
    } else {
        // Conservative default for unknown models
        128_000
    }
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens: default_max_tokens(),
            context_window: default_context_window(),
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            compact_threshold: default_compact_threshold(),
        }
    }
}

impl CompletionConfig {
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        let context_window = context_window_for_model(&model);
        Self { model, context_window, ..Default::default() }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_context_window(mut self, context_window: u32) -> Self {
        self.context_window = context_window;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_compact_threshold(mut self, threshold: f32) -> Self {
        self.compact_threshold = threshold;
        self
    }

    /// Maximum input tokens available after reserving space for the response.
    pub fn input_budget(&self) -> u32 {
        self.context_window.saturating_sub(self.max_tokens)
    }

    /// Token threshold at which compaction is triggered.
    pub fn compact_budget(&self) -> u32 {
        (self.context_window as f64 * self.compact_threshold as f64) as u32
    }
}

/// Request to the LLM provider.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub config: CompletionConfig,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_results: Vec<ToolResult>,
}

impl CompletionRequest {
    pub fn new(config: CompletionConfig, messages: Vec<Message>) -> Self {
        Self {
            config,
            system: None,
            messages,
            tools: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_tool_results(mut self, results: Vec<ToolResult>) -> Self {
        self.tool_results = results;
        self
    }
}

/// A chunk of streamed content from an LLM provider.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    Thinking(String),
    ToolUseStart { id: String, name: String },
    ToolUseInput(String),
    Complete(Response),
    Error(String),
}

/// Trait for LLM providers.
#[trait_variant::make(Send)]
pub trait Provider: Sync {
    /// Returns the name of this provider.
    fn name(&self) -> &str;

    /// Returns available models for this provider.
    fn models(&self) -> &[&str];

    /// Complete a conversation and return the response.
    async fn complete(&self, request: CompletionRequest) -> std::result::Result<Response, AgentError>;

    /// Check if the provider is configured and ready.
    fn is_available(&self) -> bool {
        true
    }

    /// Check if this provider supports streaming.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Complete with streaming.
    ///
    /// Implementations that don't support native streaming can use
    /// [`streaming_from_complete`] as a fallback that wraps `complete()`.
    async fn complete_streaming(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<mpsc::UnboundedReceiver<StreamChunk>, AgentError>;
}

/// Fallback streaming implementation that wraps a non-streaming response.
///
/// Providers that don't support native streaming can call this from their
/// `complete_streaming` implementation:
///
/// ```ignore
/// async fn complete_streaming(&self, request: CompletionRequest)
///     -> Result<mpsc::UnboundedReceiver<StreamChunk>, AgentError>
/// {
///     let response = self.complete(request).await?;
///     Ok(streaming_from_complete(response))
/// }
/// ```
pub fn streaming_from_complete(response: Response) -> mpsc::UnboundedReceiver<StreamChunk> {
    let (tx, rx) = mpsc::unbounded_channel();

    for content in &response.content {
        if let Some(text) = content.as_text() {
            let _ = tx.send(StreamChunk::Text(text.to_string()));
        } else if let Some(thinking) = content.as_thinking() {
            let _ = tx.send(StreamChunk::Thinking(thinking.to_string()));
        }
    }
    let _ = tx.send(StreamChunk::Complete(response));

    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Content, Role, StopReason, Usage};

    #[test]
    fn completion_config_default() {
        let config = CompletionConfig::default();
        assert!(config.model.is_empty());
        assert_eq!(config.max_tokens, 4096);
        assert!(config.temperature.is_none());
        assert!(config.top_p.is_none());
        assert!(config.stop_sequences.is_empty());
    }

    #[test]
    fn completion_config_new() {
        let config = CompletionConfig::new("claude-sonnet-4-6");
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.max_tokens, 4096);
    }

    #[test]
    fn completion_config_builder_pattern() {
        let config = CompletionConfig::new("claude-sonnet-4-6")
            .with_max_tokens(8192)
            .with_temperature(0.7);

        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.temperature, Some(0.7));
    }

    #[test]
    fn completion_config_serialization() {
        let config = CompletionConfig::new("claude-sonnet-4-6")
            .with_max_tokens(2048)
            .with_temperature(0.5);

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"model\":\"claude-sonnet-4-6\""));
        assert!(json.contains("\"max_tokens\":2048"));
        assert!(json.contains("\"temperature\":0.5"));
    }

    #[test]
    fn completion_config_deserialization() {
        let json = r#"{
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "temperature": 0.8
        }"#;

        let config: CompletionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.max_tokens, 1024);
        assert_eq!(config.temperature, Some(0.8));
    }

    #[test]
    fn completion_config_deserialization_defaults() {
        let json = r#"{"model": "test-model"}"#;

        let config: CompletionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.model, "test-model");
        assert_eq!(config.max_tokens, 4096); // default
        assert!(config.temperature.is_none());
    }

    #[test]
    fn completion_request_new() {
        let config = CompletionConfig::new("test-model");
        let messages = vec![Message {
            role: Role::User,
            content: vec![Content::text("Hello")],
        }];

        let request = CompletionRequest::new(config, messages);
        assert_eq!(request.config.model, "test-model");
        assert_eq!(request.messages.len(), 1);
        assert!(request.system.is_none());
        assert!(request.tools.is_empty());
    }

    #[test]
    fn completion_request_builder_pattern() {
        let config = CompletionConfig::new("test-model");
        let messages = vec![Message {
            role: Role::User,
            content: vec![Content::text("Hello")],
        }];

        let request = CompletionRequest::new(config, messages)
            .with_system("You are a helpful assistant.")
            .with_tools(vec![]);

        assert_eq!(request.system, Some("You are a helpful assistant.".to_string()));
    }

    #[test]
    fn streaming_from_complete_text() {
        let response = Response {
            content: vec![Content::text("Hello, world!")],
            tool_calls: vec![],
            stop_reason: Some(StopReason::EndTurn),
            usage: Some(Usage::default()),
        };

        let mut rx = streaming_from_complete(response);

        // Should receive text chunk
        let chunk = rx.try_recv().unwrap();
        match chunk {
            StreamChunk::Text(text) => assert_eq!(text, "Hello, world!"),
            _ => panic!("Expected Text chunk"),
        }

        // Should receive complete chunk
        let chunk = rx.try_recv().unwrap();
        assert!(matches!(chunk, StreamChunk::Complete(_)));
    }

    #[test]
    fn streaming_from_complete_thinking() {
        let response = Response {
            content: vec![
                Content::thinking("Let me think..."),
                Content::text("The answer is 42."),
            ],
            tool_calls: vec![],
            stop_reason: Some(StopReason::EndTurn),
            usage: Some(Usage::default()),
        };

        let mut rx = streaming_from_complete(response);

        // Should receive thinking chunk
        let chunk = rx.try_recv().unwrap();
        match chunk {
            StreamChunk::Thinking(text) => assert_eq!(text, "Let me think..."),
            _ => panic!("Expected Thinking chunk"),
        }

        // Should receive text chunk
        let chunk = rx.try_recv().unwrap();
        match chunk {
            StreamChunk::Text(text) => assert_eq!(text, "The answer is 42."),
            _ => panic!("Expected Text chunk"),
        }

        // Should receive complete chunk
        let chunk = rx.try_recv().unwrap();
        assert!(matches!(chunk, StreamChunk::Complete(_)));
    }

    #[test]
    fn streaming_from_complete_empty_response() {
        let response = Response {
            content: vec![],
            tool_calls: vec![],
            stop_reason: Some(StopReason::EndTurn),
            usage: Some(Usage::default()),
        };

        let mut rx = streaming_from_complete(response);

        // Should only receive complete chunk
        let chunk = rx.try_recv().unwrap();
        assert!(matches!(chunk, StreamChunk::Complete(_)));
    }
}
