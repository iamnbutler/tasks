//! Session management for multi-turn conversations.
//!
//! A session maintains conversation history and state across multiple prompt turns.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::AgentError;
use crate::message::{Content, Message, Role, Response, Tool, ToolCall, ToolResult, Usage};
use crate::provider::{CompletionConfig, CompletionRequest, Provider};

/// Unique session identifier.
pub type SessionId = String;

/// State of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Ready,
    Processing,
    AwaitingToolResults,
    Cancelled,
}

/// A conversation session.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub state: SessionState,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub pending_tool_calls: Vec<ToolCall>,
    pub pending_tool_results: Vec<ToolResult>,
    pub config: CompletionConfig,
    pub total_usage: Usage,
    pub metadata: HashMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

impl Session {
    pub fn new(config: CompletionConfig) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            state: SessionState::Ready,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
            pending_tool_calls: Vec::new(),
            pending_tool_results: Vec::new(),
            config,
            total_usage: Usage::default(),
            metadata: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    pub fn with_id(id: impl Into<String>, config: CompletionConfig) -> Self {
        Self { id: id.into(), ..Self::new(config) }
    }

    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = Some(prompt.into());
    }

    pub fn add_tool(&mut self, tool: Tool) {
        self.tools.push(tool);
    }

    pub fn set_tools(&mut self, tools: Vec<Tool>) {
        self.tools = tools;
    }

    pub fn add_user_message(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
    }

    pub fn add_assistant_message(&mut self, text: impl Into<String>) {
        self.messages.push(Message::assistant(text));
    }

    pub fn add_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    pub fn clear_history(&mut self) {
        self.messages.clear();
        self.pending_tool_calls.clear();
        self.pending_tool_results.clear();
        self.state = SessionState::Ready;
    }

    pub fn has_pending_tool_calls(&self) -> bool {
        !self.pending_tool_calls.is_empty()
    }

    pub fn pending_tool_calls(&self) -> &[ToolCall] {
        &self.pending_tool_calls
    }

    /// Build a completion request from the current session state.
    ///
    /// Consumes pending_tool_results — they are added to message history
    /// as a user message with ToolResult content blocks, then cleared.
    pub fn build_request(&mut self) -> CompletionRequest {
        if !self.pending_tool_results.is_empty() {
            let tool_result_content: Vec<Content> = std::mem::take(&mut self.pending_tool_results)
                .into_iter()
                .map(|r| Content::tool_result(r.tool_call_id, r.content, r.is_error))
                .collect();
            self.messages.push(Message::new(Role::User, tool_result_content));
        }

        let mut request = CompletionRequest::new(self.config.clone(), self.messages.clone());

        if let Some(ref system) = self.system_prompt {
            request = request.with_system(system.clone());
        }

        if !self.tools.is_empty() {
            request = request.with_tools(self.tools.clone());
        }

        request
    }

    /// Apply a response to the session, updating state.
    pub fn apply_response(&mut self, response: &Response) {
        if !response.content.is_empty() {
            self.messages.push(Message::new(Role::Assistant, response.content.clone()));
        }

        self.pending_tool_calls = response.tool_calls.clone();

        if !self.pending_tool_calls.is_empty() {
            self.state = SessionState::AwaitingToolResults;
        } else {
            self.state = SessionState::Ready;
        }

        if let Some(usage) = response.usage {
            self.total_usage.input_tokens += usage.input_tokens;
            self.total_usage.output_tokens += usage.output_tokens;
        }
    }

    /// Apply tool results and prepare for the next turn.
    pub fn apply_tool_results(&mut self, results: Vec<ToolResult>) {
        let result_text = results.iter()
            .map(|r| {
                if r.is_error {
                    format!("Tool {} error: {}", r.tool_call_id, r.content)
                } else {
                    format!("Tool {} result: {}", r.tool_call_id, r.content)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        self.messages.push(Message::user(result_text));
        self.pending_tool_calls.clear();
        self.state = SessionState::Ready;
    }

    pub fn cancel(&mut self) {
        self.state = SessionState::Cancelled;
        self.pending_tool_calls.clear();
    }
}

/// Builder for creating and configuring sessions.
pub struct SessionBuilder {
    config: CompletionConfig,
    system_prompt: Option<String>,
    tools: Vec<Tool>,
    initial_messages: Vec<Message>,
}

impl SessionBuilder {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            config: CompletionConfig::new(model),
            system_prompt: None,
            tools: Vec::new(),
            initial_messages: Vec::new(),
        }
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.config.max_tokens = max_tokens;
        self
    }

    pub fn temperature(mut self, temperature: f32) -> Self {
        self.config.temperature = Some(temperature);
        self
    }

    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    pub fn message(mut self, message: Message) -> Self {
        self.initial_messages.push(message);
        self
    }

    pub fn build(self) -> Session {
        let mut session = Session::new(self.config);
        session.system_prompt = self.system_prompt;
        session.tools = self.tools;
        session.messages = self.initial_messages;
        session
    }
}

/// Simple chain for multi-turn conversations.
///
/// For more complex chains with validation gates and programmatic steps,
/// use [`crate::chain::ChainBuilder`].
pub struct Chain<P: Provider> {
    provider: Arc<P>,
    session: Session,
}

impl<P: Provider> Chain<P> {
    pub fn new(provider: Arc<P>, session: Session) -> Self {
        Self { provider, session }
    }

    pub fn with_model(provider: Arc<P>, model: impl Into<String>) -> Self {
        Self::new(provider, Session::new(CompletionConfig::new(model)))
    }

    pub fn system(mut self, prompt: impl Into<String>) -> Self {
        self.session.set_system_prompt(prompt);
        self
    }

    pub fn tools(mut self, tools: Vec<Tool>) -> Self {
        self.session.set_tools(tools);
        self
    }

    pub async fn user(mut self, message: impl Into<String>) -> Result<Self, AgentError> {
        self.session.add_user_message(message);
        self.session.state = SessionState::Processing;

        let request = self.session.build_request();
        let response = self.provider.complete(request).await?;
        self.session.apply_response(&response);

        Ok(self)
    }

    pub async fn user_validated<F>(
        mut self,
        message: impl Into<String>,
        validator: F,
    ) -> Result<Self, AgentError>
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        self.session.add_user_message(message);
        self.session.state = SessionState::Processing;

        let request = self.session.build_request();
        let response = self.provider.complete(request).await?;

        let response_text = response.text();
        validator(&response_text).map_err(|e| AgentError::Other(format!("Validation failed: {}", e)))?;

        self.session.apply_response(&response);

        Ok(self)
    }

    pub fn last_response(&self) -> Option<String> {
        self.session.messages.iter().rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.text())
    }

    pub fn history(&self) -> &[Message] {
        self.session.history()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn into_session(self) -> Session {
        self.session
    }

    pub fn pending_tool_calls(&self) -> &[ToolCall] {
        self.session.pending_tool_calls()
    }

    pub async fn tool_results(mut self, results: Vec<ToolResult>) -> Result<Self, AgentError> {
        self.session.apply_tool_results(results);
        self.session.state = SessionState::Processing;

        let request = self.session.build_request();
        let response = self.provider.complete(request).await?;
        self.session.apply_response(&response);

        Ok(self)
    }

    pub fn usage(&self) -> Usage {
        self.session.total_usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Content, StopReason};

    #[test]
    fn test_session_new() {
        let session = Session::new(CompletionConfig::new("test-model"));
        assert_eq!(session.state, SessionState::Ready);
        assert!(session.messages.is_empty());
        assert!(!session.id.is_empty());
    }

    #[test]
    fn test_session_add_messages() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.add_user_message("hello");
        session.add_assistant_message("hi there");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text(), "hello");
        assert_eq!(session.messages[1].text(), "hi there");
    }

    #[test]
    fn test_session_build_request() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.set_system_prompt("be helpful");
        session.add_user_message("hello");
        let request = session.build_request();
        assert_eq!(request.config.model, "test-model");
        assert_eq!(request.system, Some("be helpful".to_string()));
        assert_eq!(request.messages.len(), 1);
    }

    #[test]
    fn test_session_apply_response() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        let response = Response {
            content: vec![Content::text("hello back")],
            tool_calls: vec![],
            stop_reason: Some(StopReason::EndTurn),
            usage: Some(Usage { input_tokens: 10, output_tokens: 5 }),
        };
        session.apply_response(&response);
        assert_eq!(session.state, SessionState::Ready);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.total_usage.input_tokens, 10);
    }

    #[test]
    fn test_session_apply_response_with_tool_calls() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        let response = Response {
            content: vec![Content::text("let me check")],
            tool_calls: vec![ToolCall {
                id: "tc-1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "/tmp"}),
            }],
            stop_reason: Some(StopReason::ToolUse),
            usage: Some(Usage { input_tokens: 10, output_tokens: 5 }),
        };
        session.apply_response(&response);
        assert_eq!(session.state, SessionState::AwaitingToolResults);
        assert!(session.has_pending_tool_calls());
    }

    #[test]
    fn test_session_clear_history() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.add_user_message("hello");
        session.clear_history();
        assert!(session.messages.is_empty());
        assert_eq!(session.state, SessionState::Ready);
    }

    #[test]
    fn test_session_builder() {
        let session = SessionBuilder::new("claude-sonnet-4-20250514")
            .system_prompt("be helpful")
            .max_tokens(1024)
            .temperature(0.7)
            .build();
        assert_eq!(session.config.model, "claude-sonnet-4-20250514");
        assert_eq!(session.system_prompt, Some("be helpful".to_string()));
        assert_eq!(session.config.max_tokens, 1024);
        assert_eq!(session.config.temperature, Some(0.7));
    }

    #[test]
    fn test_session_cancel() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.cancel();
        assert_eq!(session.state, SessionState::Cancelled);
    }
}
