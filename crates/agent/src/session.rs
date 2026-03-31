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

/// Maximum consecutive compaction failures before backing off.
const MAX_COMPACTION_FAILURES: u32 = 3;

/// Fraction of context window that triggers compaction (85%).
const COMPACTION_THRESHOLD: f32 = 0.85;

/// Minimum number of recent messages to preserve after compaction.
const COMPACTION_KEEP_RECENT: usize = 10;

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
    /// Number of times compaction has been performed in this session.
    pub compaction_count: u32,
    /// Consecutive compaction failures (resets on success). Used for backoff.
    compaction_failures: u32,
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
            compaction_count: 0,
            compaction_failures: 0,
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
    ///
    /// If the conversation exceeds the model's context window, older messages
    /// are dropped (keeping the first user message for task context and recent
    /// messages for continuity). A warning is logged when truncation occurs.
    pub fn build_request(&mut self) -> CompletionRequest {
        if !self.pending_tool_results.is_empty() {
            let tool_result_content: Vec<Content> = std::mem::take(&mut self.pending_tool_results)
                .into_iter()
                .map(|r| Content::tool_result(r.tool_call_id, r.content, r.is_error))
                .collect();
            self.messages.push(Message::new(Role::User, tool_result_content));
        }

        let messages = self.truncate_to_budget();

        let mut request = CompletionRequest::new(self.config.clone(), messages);

        if let Some(ref system) = self.system_prompt {
            request = request.with_system(system.clone());
        }

        if !self.tools.is_empty() {
            request = request.with_tools(self.tools.clone());
        }

        request
    }

    /// Compact conversation history via LLM summarization if context usage exceeds threshold.
    ///
    /// Returns `Ok(true)` if compaction was performed, `Ok(false)` if not needed or skipped.
    /// Compaction replaces older messages with a system-role summary, preserving the first
    /// user message (task context) and recent messages for continuity.
    ///
    /// The summary is inserted as a `Role::System` message to avoid role alternation
    /// violations (the first message is typically `Role::User`).
    pub async fn compact_if_needed(&mut self, provider: &(impl Provider + ?Sized)) -> Result<bool, AgentError> {
        // Back off after repeated failures
        if self.compaction_failures >= MAX_COMPACTION_FAILURES {
            tracing::debug!(
                session_id = %self.id,
                failures = self.compaction_failures,
                "skipping compaction after repeated failures"
            );
            return Ok(false);
        }

        let total_tokens: u32 = self.messages.iter().map(|m| m.estimate_tokens()).sum();
        let threshold = (self.config.context_window as f32 * COMPACTION_THRESHOLD) as u32;

        if total_tokens <= threshold {
            return Ok(false);
        }

        // Not enough messages to meaningfully compact
        if self.messages.len() <= COMPACTION_KEEP_RECENT + 1 {
            return Ok(false);
        }

        tracing::info!(
            session_id = %self.id,
            total_tokens,
            threshold,
            total_messages = self.messages.len(),
            "compacting conversation history via summarization"
        );

        // Determine which messages to summarize vs keep
        let keep_recent = self.messages.len().saturating_sub(COMPACTION_KEEP_RECENT);
        let messages_to_summarize = &self.messages[..keep_recent];

        match self.summarize_conversation(provider, messages_to_summarize).await {
            Ok(summary) => {
                let recent = self.messages[keep_recent..].to_vec();

                // Rebuild messages: [first_user_msg, system_summary, assistant_ack, ...recent]
                // Using Role::System for the summary avoids consecutive User messages.
                let mut new_messages = Vec::with_capacity(3 + recent.len());
                new_messages.push(self.messages[0].clone());
                new_messages.push(Message::system(format!(
                    "[Conversation Summary — compaction #{}]\n{}",
                    self.compaction_count + 1,
                    summary
                )));
                new_messages.push(Message::assistant(
                    "Understood. I have the conversation context from the summary and will continue from here."
                ));
                new_messages.extend(recent);

                // Ensure valid role alternation at the boundary.
                // If recent starts with an Assistant message, we'd have two consecutive
                // Assistant messages. In that case, drop the ack.
                if new_messages.len() > 3 && new_messages[3].role == Role::Assistant {
                    new_messages.remove(2);
                }

                self.messages = new_messages;
                self.compaction_count += 1;
                self.compaction_failures = 0;

                tracing::info!(
                    session_id = %self.id,
                    compaction_count = self.compaction_count,
                    new_message_count = self.messages.len(),
                    "compaction complete"
                );

                Ok(true)
            }
            Err(e) => {
                self.compaction_failures += 1;
                tracing::warn!(
                    session_id = %self.id,
                    error = %e,
                    failures = self.compaction_failures,
                    max_failures = MAX_COMPACTION_FAILURES,
                    "compaction summarization failed"
                );
                Ok(false)
            }
        }
    }

    /// Summarize a slice of conversation messages using the provider.
    async fn summarize_conversation(
        &self,
        provider: &(impl Provider + ?Sized),
        messages: &[Message],
    ) -> Result<String, AgentError> {
        // Format messages into a readable transcript
        let mut transcript = String::new();
        for msg in messages {
            let role_label = match msg.role {
                Role::System => "System",
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            let text = msg.text();
            if !text.is_empty() {
                transcript.push_str(&format!("[{}]: {}\n\n", role_label, text));
            }
            // Note: tool use/result details are omitted from the transcript to save
            // tokens. The summary captures the semantic content, not raw tool output.
            for content in &msg.content {
                match content {
                    Content::ToolUse { name, .. } => {
                        transcript.push_str(&format!("[{} used tool: {}]\n\n", role_label, name));
                    }
                    Content::ToolResult { is_error, .. } if *is_error => {
                        transcript.push_str("[Tool returned an error]\n\n");
                    }
                    _ => {}
                }
            }
        }

        let system_prompt = r#"You are a conversation summarizer. Summarize the following conversation transcript, preserving:
- Key decisions made and their reasoning
- Important facts, file paths, code changes, and technical details
- Current task state and what has been accomplished
- Any errors encountered and how they were resolved
- Outstanding work or next steps mentioned

Be concise but thorough. Focus on information the assistant will need to continue working effectively.
Return ONLY the summary, no preamble."#;

        let config = CompletionConfig::new(&self.config.model)
            .with_max_tokens(1024);
        let request = CompletionRequest::new(config, vec![Message::user(transcript)])
            .with_system(system_prompt);

        let response = provider.complete(request).await?;
        Ok(response.text().trim().to_string())
    }

    /// Truncate messages to fit within the context window budget.
    ///
    /// Strategy: keep the first user message (task context) and as many
    /// recent messages as fit. Drop from the middle when over budget.
    fn truncate_to_budget(&self) -> Vec<Message> {
        let budget = self.config.input_budget();

        // Account for system prompt and tool definitions
        let mut overhead: u32 = 0;
        if let Some(ref system) = self.system_prompt {
            overhead += (system.len() as u32) / 4 + 10;
        }
        for tool in &self.tools {
            overhead += (tool.description.len() as u32 + tool.parameters.to_string().len() as u32) / 4 + 20;
        }

        let available = budget.saturating_sub(overhead);

        // Fast path: estimate total and skip truncation if under budget
        let total_tokens: u32 = self.messages.iter().map(|m| m.estimate_tokens()).sum();
        if total_tokens <= available {
            return self.messages.clone();
        }

        // Need to truncate. Keep first message (task context) + recent messages.
        let total = self.messages.len();
        if total <= 2 {
            // Too few messages to truncate meaningfully
            return self.messages.clone();
        }

        // Reserve first message
        let first_tokens = self.messages[0].estimate_tokens();
        let remaining_budget = available.saturating_sub(first_tokens);

        // Walk backwards from the end, accumulating messages that fit
        let mut tail_tokens: u32 = 0;
        let mut keep_from = total; // exclusive start of tail
        for i in (1..total).rev() {
            let msg_tokens = self.messages[i].estimate_tokens();
            if tail_tokens + msg_tokens > remaining_budget {
                break;
            }
            tail_tokens += msg_tokens;
            keep_from = i;
        }

        // Ensure we don't break ToolUse/ToolResult pairing.
        // The Anthropic API requires every ToolResult user message to be
        // preceded by the assistant message containing the corresponding
        // ToolUse. If the tail starts with an orphaned ToolResult, advance
        // past it to find a safe boundary.
        let keep_from_before_orphan_skip = keep_from;
        while keep_from < total {
            let msg = &self.messages[keep_from];
            let is_orphan_tool_result = msg.role == Role::User
                && msg.content.iter().all(|c| matches!(c, Content::ToolResult { .. }));
            if !is_orphan_tool_result {
                break;
            }
            // Drop the orphaned ToolResult and reclaim its tokens
            tail_tokens = tail_tokens.saturating_sub(msg.estimate_tokens());
            keep_from += 1;
        }

        let budget_dropped = keep_from_before_orphan_skip.saturating_sub(1);
        let orphan_skipped = keep_from - keep_from_before_orphan_skip;
        if budget_dropped + orphan_skipped > 0 {
            tracing::warn!(
                session_id = %self.id,
                total_messages = total,
                budget_dropped,
                orphan_skipped,
                estimated_tokens = total_tokens,
                budget = available,
                "truncated conversation history to fit context window"
            );
        }

        let mut result = Vec::with_capacity(1 + (total - keep_from));
        result.push(self.messages[0].clone());
        if keep_from < total {
            result.extend_from_slice(&self.messages[keep_from..]);
        }
        result
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
    ///
    /// Stores results in `pending_tool_results` so that `build_request()`
    /// emits them as proper `Content::ToolResult` blocks (required by the
    /// Anthropic API), rather than plain text.
    pub fn apply_tool_results(&mut self, results: Vec<ToolResult>) {
        self.pending_tool_results = results;
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
    use tokio::sync::mpsc;

    /// A mock provider that returns a canned response for testing compaction.
    struct MockProvider {
        response_text: String,
    }

    impl MockProvider {
        fn new(text: impl Into<String>) -> Self {
            Self { response_text: text.into() }
        }
    }

    impl Provider for MockProvider {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[&str] { &["mock-model"] }
        async fn complete(&self, _request: CompletionRequest) -> std::result::Result<Response, AgentError> {
            Ok(Response {
                content: vec![Content::text(&self.response_text)],
                tool_calls: vec![],
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(Usage { input_tokens: 10, output_tokens: 20 }),
            })
        }
        fn supports_streaming(&self) -> bool { false }
        async fn complete_streaming(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<mpsc::UnboundedReceiver<crate::provider::StreamChunk>, AgentError> {
            Err(AgentError::Other("not supported".into()))
        }
    }

    /// A mock provider that always fails, for testing compaction failure backoff.
    struct FailingProvider;

    impl Provider for FailingProvider {
        fn name(&self) -> &str { "failing" }
        fn models(&self) -> &[&str] { &[] }
        async fn complete(&self, _request: CompletionRequest) -> std::result::Result<Response, AgentError> {
            Err(AgentError::Provider("summarization failed".into()))
        }
        fn supports_streaming(&self) -> bool { false }
        async fn complete_streaming(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<mpsc::UnboundedReceiver<crate::provider::StreamChunk>, AgentError> {
            Err(AgentError::Other("not supported".into()))
        }
    }

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
        let session = SessionBuilder::new("claude-sonnet-4-6")
            .system_prompt("be helpful")
            .max_tokens(1024)
            .temperature(0.7)
            .build();
        assert_eq!(session.config.model, "claude-sonnet-4-6");
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

    #[test]
    fn test_apply_tool_results_emits_structured_content() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.add_user_message("hello");

        // Simulate a response with tool calls
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

        // Apply tool results — should go to pending_tool_results, not as text
        session.apply_tool_results(vec![
            ToolResult::success("tc-1", "file contents here"),
        ]);

        assert_eq!(session.state, SessionState::Ready);
        assert!(!session.has_pending_tool_calls());
        assert_eq!(session.pending_tool_results.len(), 1);

        // build_request should emit proper ToolResult content blocks
        let request = session.build_request();
        let last_msg = request.messages.last().unwrap();
        assert_eq!(last_msg.role, Role::User);
        assert!(matches!(&last_msg.content[0], Content::ToolResult { tool_use_id, .. } if tool_use_id == "tc-1"));
    }

    #[test]
    fn test_truncation_under_budget_no_change() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.add_user_message("hello");
        session.add_assistant_message("hi");
        session.add_user_message("how are you?");
        let request = session.build_request();
        assert_eq!(request.messages.len(), 3);
    }

    #[test]
    fn test_truncation_over_budget_drops_middle() {
        // Create a session with a tiny context window
        let config = CompletionConfig::new("test-model")
            .with_context_window(100) // ~100 tokens total
            .with_max_tokens(20);     // 80 tokens for input
        let mut session = Session::new(config);

        // First message (kept as task context)
        session.add_user_message("task: implement feature X");
        // Middle messages (candidates for dropping)
        for i in 0..20 {
            session.add_assistant_message(&format!("working on step {} of the implementation with lots of detail", i));
            session.add_user_message(&format!("continue with step {}", i + 1));
        }
        // The conversation is now much larger than 80 tokens

        let request = session.build_request();
        // Should be truncated: first message + recent messages
        assert!(request.messages.len() < session.messages.len());
        // First message preserved
        assert_eq!(request.messages[0].text(), "task: implement feature X");
        // Last message preserved
        let last = request.messages.last().unwrap();
        assert!(last.text().contains("continue with step"));
    }

    #[test]
    fn test_truncation_preserves_first_and_last() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(60)
            .with_max_tokens(10);
        let mut session = Session::new(config);

        session.add_user_message("initial task context");
        session.add_assistant_message("understood, I'll start working");
        session.add_user_message("great, proceed");
        session.add_assistant_message("done with part 1, here are the results of my work so far");
        session.add_user_message("looks good, continue");

        let request = session.build_request();
        // First message must be the initial context
        assert_eq!(request.messages[0].text(), "initial task context");
        // Last message must be the most recent
        assert_eq!(request.messages.last().unwrap().text(), "looks good, continue");
    }

    #[test]
    fn test_context_window_inferred_from_model() {
        let config = CompletionConfig::new("claude-sonnet-4-6");
        assert_eq!(config.context_window, 200_000);

        let config = CompletionConfig::new("unknown-model");
        assert_eq!(config.context_window, 128_000);
    }

    #[test]
    fn test_input_budget() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(200_000)
            .with_max_tokens(4096);
        assert_eq!(config.input_budget(), 200_000 - 4096);
    }

    #[test]
    fn test_truncation_skips_orphaned_tool_results() {
        // Use a very small context window to force truncation
        let config = CompletionConfig::new("test-model")
            .with_context_window(50)
            .with_max_tokens(10);
        let mut session = Session::new(config);

        // [0] User: task context (kept)
        session.add_user_message("implement feature X");
        // [1] Assistant with ToolUse (will be dropped — middle)
        session.messages.push(Message::new(Role::Assistant, vec![
            Content::ToolUse {
                id: "tc-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/foo.rs"}),
            },
        ]));
        // [2] User with ToolResult (orphaned if [1] is dropped)
        session.messages.push(Message::new(Role::User, vec![
            Content::tool_result("tc-1", "lots of file contents here that take up space in the context", false),
        ]));
        // [3] Assistant: normal text
        session.add_assistant_message("ok, I see the file and will proceed");
        // [4] User: normal text
        session.add_user_message("great, continue with the implementation");

        let request = session.build_request();

        // The tail should NOT start with the orphaned ToolResult [2].
        // It should skip it and start at [3] or [4].
        for msg in &request.messages[1..] {
            let is_tool_result_only = msg.role == Role::User
                && msg.content.iter().all(|c| matches!(c, Content::ToolResult { .. }));
            assert!(
                !is_tool_result_only,
                "tail contains orphaned ToolResult message"
            );
        }
        // First message preserved
        assert_eq!(request.messages[0].text(), "implement feature X");
    }

    #[tokio::test]
    async fn test_compact_if_needed_under_threshold() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.add_user_message("hello");
        session.add_assistant_message("hi");

        let provider = MockProvider::new("summary");
        let result = session.compact_if_needed(&provider).await.unwrap();
        assert!(!result, "should not compact when under threshold");
        assert_eq!(session.compaction_count, 0);
    }

    #[tokio::test]
    async fn test_compact_if_needed_over_threshold() {
        // Use a tiny context window to force compaction
        let config = CompletionConfig::new("test-model")
            .with_context_window(200)
            .with_max_tokens(20);
        let mut session = Session::new(config);

        // Add enough messages to exceed 85% of 200 = 170 tokens
        session.add_user_message("task: implement a complex feature with many requirements");
        for i in 0..20 {
            session.add_assistant_message(&format!(
                "working on step {} with detailed explanation of what I am doing", i
            ));
            session.add_user_message(&format!("continue with step {} please", i + 1));
        }

        let original_count = session.messages.len();
        let provider = MockProvider::new("Summary of the conversation so far.");
        let result = session.compact_if_needed(&provider).await.unwrap();
        assert!(result, "should compact when over threshold");
        assert_eq!(session.compaction_count, 1);
        assert!(session.messages.len() < original_count);

        // First message should still be the original task context
        assert_eq!(session.messages[0].role, Role::User);
        assert!(session.messages[0].text().contains("implement a complex feature"));

        // Second message should be the system summary
        assert_eq!(session.messages[1].role, Role::System);
        assert!(session.messages[1].text().contains("Conversation Summary"));
        assert!(session.messages[1].text().contains("Summary of the conversation so far."));
    }

    #[tokio::test]
    async fn test_compact_role_alternation_no_consecutive_user() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(200)
            .with_max_tokens(20);
        let mut session = Session::new(config);

        session.add_user_message("task context");
        for i in 0..20 {
            session.add_assistant_message(&format!("step {} detailed work output", i));
            session.add_user_message(&format!("continue with step {}", i + 1));
        }

        let provider = MockProvider::new("summary text");
        session.compact_if_needed(&provider).await.unwrap();

        // Verify no two consecutive messages have the same role
        // (except System which is handled differently by providers)
        for window in session.messages.windows(2) {
            let r0 = window[0].role;
            let r1 = window[1].role;
            // User-User and Assistant-Assistant are forbidden
            assert!(
                !(r0 == Role::User && r1 == Role::User),
                "consecutive User messages at roles {:?} -> {:?}", r0, r1
            );
            assert!(
                !(r0 == Role::Assistant && r1 == Role::Assistant),
                "consecutive Assistant messages at roles {:?} -> {:?}", r0, r1
            );
        }
    }

    #[tokio::test]
    async fn test_compact_failure_backoff() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(200)
            .with_max_tokens(20);
        let mut session = Session::new(config);

        // Add enough messages to exceed threshold
        session.add_user_message("task context");
        for i in 0..20 {
            session.add_assistant_message(&format!("step {} detailed output", i));
            session.add_user_message(&format!("continue {}", i));
        }

        let provider = FailingProvider;

        // Each call should fail gracefully (return Ok(false)) and increment failure count
        for i in 0..MAX_COMPACTION_FAILURES {
            let result = session.compact_if_needed(&provider).await.unwrap();
            assert!(!result);
            assert_eq!(session.compaction_failures, i + 1);
        }

        // After MAX_COMPACTION_FAILURES, it should skip without even trying
        let result = session.compact_if_needed(&provider).await.unwrap();
        assert!(!result);
        // Failure count should not increase further (we didn't attempt)
        assert_eq!(session.compaction_failures, MAX_COMPACTION_FAILURES);
    }

    #[tokio::test]
    async fn test_compact_resets_failure_count_on_success() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(200)
            .with_max_tokens(20);
        let mut session = Session::new(config);

        // Simulate some prior failures
        session.compaction_failures = 2;

        // Add messages to exceed threshold
        session.add_user_message("task context");
        for i in 0..20 {
            session.add_assistant_message(&format!("step {} output", i));
            session.add_user_message(&format!("continue {}", i));
        }

        let provider = MockProvider::new("summary");
        let result = session.compact_if_needed(&provider).await.unwrap();
        assert!(result);
        assert_eq!(session.compaction_failures, 0, "failures should reset on success");
        assert_eq!(session.compaction_count, 1);
    }

    #[tokio::test]
    async fn test_compact_too_few_messages() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(10) // tiny window
            .with_max_tokens(1);
        let mut session = Session::new(config);

        // Only a few messages — not enough to justify compaction
        session.add_user_message("task");
        session.add_assistant_message("ok");

        let provider = MockProvider::new("summary");
        let result = session.compact_if_needed(&provider).await.unwrap();
        assert!(!result, "should not compact when too few messages");
    }
}
