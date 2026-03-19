//! Completions service for lightweight LLM tasks.
//!
//! Uses Claude Haiku for fast, cost-effective completions like naming,
//! descriptions, brainstorming, and summarization.

use crate::error::AgentError;
use crate::message::Message;
use crate::provider::{CompletionConfig, CompletionRequest};
use crate::providers::AnthropicProvider;
use crate::Provider;

/// The Haiku model ID for fast completions.
pub(crate) const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// Service for lightweight LLM completions using Claude Haiku.
#[derive(Debug, Clone)]
pub struct CompletionsService {
    provider: AnthropicProvider,
}

impl CompletionsService {
    /// Create a new completions service with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            provider: AnthropicProvider::new(api_key),
        }
    }

    /// Create a completions service from the ANTHROPIC_API_KEY environment variable.
    pub fn from_env() -> Result<Self, AgentError> {
        Ok(Self {
            provider: AnthropicProvider::from_env()?,
        })
    }

    /// Check if the service is available (API key is set).
    pub fn is_available(&self) -> bool {
        self.provider.is_available()
    }

    /// Execute a general completion with a custom prompt.
    pub async fn complete(&self, prompt: &str, max_tokens: Option<u32>) -> Result<String, AgentError> {
        let config = CompletionConfig::new(HAIKU_MODEL)
            .with_max_tokens(max_tokens.unwrap_or(1024));

        let request = CompletionRequest::new(
            config,
            vec![Message::user(prompt)],
        );

        let response = self.provider.complete(request).await?;
        Ok(response.text())
    }

    /// Execute a completion with a system prompt.
    pub async fn complete_with_system(
        &self,
        system: &str,
        prompt: &str,
        max_tokens: Option<u32>,
    ) -> Result<String, AgentError> {
        let config = CompletionConfig::new(HAIKU_MODEL)
            .with_max_tokens(max_tokens.unwrap_or(1024));

        let request = CompletionRequest::new(
            config,
            vec![Message::user(prompt)],
        )
        .with_system(system);

        let response = self.provider.complete(request).await?;
        Ok(response.text())
    }

    /// Generate a concise name for something (thread, task, etc.).
    ///
    /// Returns a short, descriptive name based on the provided context.
    pub async fn generate_name(&self, context: &str) -> Result<String, AgentError> {
        let system = r#"You are a naming assistant. Generate a single, concise name (3-7 words) based on the context provided.
Return ONLY the name, nothing else. No quotes, no explanation, no punctuation at the end."#;

        self.complete_with_system(system, context, Some(64)).await
            .map(|s| s.trim().to_string())
    }

    /// Generate a description for something.
    ///
    /// Returns a brief description (1-2 sentences) summarizing the context.
    pub async fn generate_description(&self, context: &str) -> Result<String, AgentError> {
        let system = r#"You are a description assistant. Generate a brief description (1-2 sentences) summarizing the context provided.
Be concise and informative. Return ONLY the description."#;

        self.complete_with_system(system, context, Some(256)).await
            .map(|s| s.trim().to_string())
    }

    /// Brainstorm ideas related to the given topic.
    ///
    /// Returns a list of ideas, one per line.
    pub async fn brainstorm(&self, topic: &str, count: Option<u32>) -> Result<Vec<String>, AgentError> {
        let count = count.unwrap_or(5);
        let system = format!(
            r#"You are a brainstorming assistant. Generate exactly {} creative ideas related to the topic.
Return one idea per line. No numbering, no bullets, no explanations - just the ideas."#,
            count
        );

        let response = self.complete_with_system(&system, topic, Some(512)).await?;
        Ok(response
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// Summarize the given text.
    ///
    /// Returns a condensed summary preserving key information.
    pub async fn summarize(&self, text: &str, max_length: Option<u32>) -> Result<String, AgentError> {
        let length_hint = max_length.map_or(String::new(), |l| format!(" Keep it under {} words.", l));
        let system = format!(
            r#"You are a summarization assistant. Summarize the provided text concisely while preserving key information.{}
Return ONLY the summary."#,
            length_hint
        );

        self.complete_with_system(&system, text, Some(512)).await
            .map(|s| s.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_haiku_model_constant() {
        assert_eq!(HAIKU_MODEL, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_completions_service_from_env_missing() {
        // Clear the env var if set for this test
        let result = std::env::var("ANTHROPIC_API_KEY");
        if result.is_err() {
            assert!(CompletionsService::from_env().is_err());
        }
    }

    #[test]
    fn test_completions_service_availability() {
        let service = CompletionsService::new("test-key");
        assert!(service.is_available());

        let empty_service = CompletionsService::new("");
        assert!(!empty_service.is_available());
    }
}
