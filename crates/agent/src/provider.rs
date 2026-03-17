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
    /// Model identifier (e.g., "claude-sonnet-4-20250514")
    pub model: String,
    /// Maximum tokens to generate
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Temperature for sampling (0.0 - 1.0)
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Top-p sampling parameter
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Stop sequences
    #[serde(default)]
    pub stop_sequences: Vec<String>,
}

fn default_max_tokens() -> u32 {
    4096
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens: default_max_tokens(),
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
        }
    }
}

impl CompletionConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self { model: model.into(), ..Default::default() }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
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
