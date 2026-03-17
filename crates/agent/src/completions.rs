//! Fast completions service for general-purpose LLM utilities.
//!
//! Provides a lightweight service using claude-haiku-4-5 for quick tasks like:
//! - Naming threads
//! - Generating descriptions
//! - Brainstorming names
//! - Other general-purpose text generation

use crate::error::AgentError;
use crate::message::{Message, Response};
use crate::provider::{CompletionConfig, CompletionRequest, Provider};
use crate::providers::AnthropicProvider;

/// Default model for fast completions (Haiku for speed).
pub const FAST_MODEL: &str = "claude-haiku-4-5-20251001";

/// Default max tokens for completions (conservative for fast responses).
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Service for fast, general-purpose LLM completions.
///
/// Uses claude-haiku-4-5 by default for quick responses. Designed for
/// utility tasks that don't require the full capabilities of larger models.
#[derive(Debug, Clone)]
pub struct CompletionsService {
    provider: AnthropicProvider,
    model: String,
    max_tokens: u32,
}

impl CompletionsService {
    /// Create a new completions service with the given provider.
    pub fn new(provider: AnthropicProvider) -> Self {
        Self {
            provider,
            model: FAST_MODEL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Create from environment variables (ANTHROPIC_API_KEY).
    pub fn from_env() -> Result<Self, AgentError> {
        let provider = AnthropicProvider::from_env()?;
        Ok(Self::new(provider))
    }

    /// Set a custom model (default is claude-haiku-4-5).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set custom max tokens (default is 1024).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Generate a simple text completion.
    ///
    /// # Arguments
    /// * `prompt` - The user prompt to complete
    ///
    /// # Returns
    /// The generated text response
    pub async fn complete(&self, prompt: &str) -> Result<String, AgentError> {
        let config = CompletionConfig::new(&self.model).with_max_tokens(self.max_tokens);
        let request = CompletionRequest::new(config, vec![Message::user(prompt)]);
        let response = self.provider.complete(request).await?;
        Ok(response.text())
    }

    /// Generate a completion with a system prompt.
    ///
    /// # Arguments
    /// * `system` - The system prompt providing context/instructions
    /// * `prompt` - The user prompt to complete
    ///
    /// # Returns
    /// The generated text response
    pub async fn complete_with_system(
        &self,
        system: &str,
        prompt: &str,
    ) -> Result<String, AgentError> {
        let config = CompletionConfig::new(&self.model).with_max_tokens(self.max_tokens);
        let request = CompletionRequest::new(config, vec![Message::user(prompt)])
            .with_system(system);
        let response = self.provider.complete(request).await?;
        Ok(response.text())
    }

    /// Generate a completion with full control over parameters.
    ///
    /// # Arguments
    /// * `system` - Optional system prompt
    /// * `messages` - Conversation messages
    /// * `temperature` - Optional temperature for sampling
    /// * `max_tokens` - Optional max tokens override
    ///
    /// # Returns
    /// The full response including usage information
    pub async fn complete_advanced(
        &self,
        system: Option<&str>,
        messages: Vec<Message>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> Result<Response, AgentError> {
        let mut config = CompletionConfig::new(&self.model)
            .with_max_tokens(max_tokens.unwrap_or(self.max_tokens));
        if let Some(temp) = temperature {
            config = config.with_temperature(temp);
        }
        let mut request = CompletionRequest::new(config, messages);
        if let Some(sys) = system {
            request = request.with_system(sys);
        }
        self.provider.complete(request).await
    }

    /// Generate a name for something.
    ///
    /// Utility method that uses a tailored system prompt for naming tasks.
    pub async fn generate_name(&self, context: &str) -> Result<String, AgentError> {
        let system = "You are a naming assistant. Generate a single, concise name based on the context provided. \
                      Return only the name with no explanation or additional text.";
        self.complete_with_system(system, context).await
    }

    /// Generate a short description.
    ///
    /// Utility method that uses a tailored system prompt for descriptions.
    pub async fn generate_description(&self, context: &str) -> Result<String, AgentError> {
        let system = "You are a description writer. Generate a clear, concise description (1-2 sentences) \
                      based on the context provided. Return only the description with no additional formatting.";
        self.complete_with_system(system, context).await
    }

    /// Brainstorm names or ideas.
    ///
    /// Utility method that returns multiple suggestions.
    ///
    /// # Arguments
    /// * `context` - What to brainstorm about
    /// * `count` - Number of suggestions to generate (default 5)
    pub async fn brainstorm(&self, context: &str, count: Option<u32>) -> Result<String, AgentError> {
        let n = count.unwrap_or(5);
        let system = format!(
            "You are a creative brainstorming assistant. Generate exactly {} unique suggestions \
             based on the context provided. Format as a numbered list, one per line. \
             Be creative but relevant.",
            n
        );
        self.complete_with_system(&system, context).await
    }

    /// Summarize text content.
    ///
    /// Utility method for quick summarization.
    pub async fn summarize(&self, content: &str) -> Result<String, AgentError> {
        let system = "You are a summarization assistant. Provide a clear, concise summary \
                      of the content provided. Focus on the key points and keep it brief.";
        self.complete_with_system(system, content).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_creation() {
        // Test that service can be configured
        let provider = AnthropicProvider::new("test-key");
        let service = CompletionsService::new(provider)
            .with_model("claude-haiku-4-5-20251001")
            .with_max_tokens(512);
        assert_eq!(service.model, "claude-haiku-4-5-20251001");
        assert_eq!(service.max_tokens, 512);
    }

    #[test]
    fn test_default_model() {
        assert_eq!(FAST_MODEL, "claude-haiku-4-5-20251001");
    }
}
