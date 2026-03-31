//! Session management for multi-turn conversations.
//!
//! A session maintains conversation history and state across multiple prompt turns.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::AgentError;
use crate::message::{Content, Message, Role, Response, Tool, ToolCall, ToolResult, Usage};
use crate::provider::{CompletionConfig, CompletionRequest, Provider};
use crate::tool_result_budget;

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
    /// Directory for persisting large tool outputs. When set, tool results
    /// exceeding their `max_result_size` are written to disk and replaced
    /// with a preview + file path.
    pub output_dir: Option<PathBuf>,
    /// Number of times compaction has been performed on this session.
    pub compaction_count: u32,
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
            output_dir: None,
            compaction_count: 0,
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

    /// Check whether the conversation exceeds the compaction threshold and,
    /// if so, summarize older messages via an LLM call.
    ///
    /// The first message (task context) and the most recent messages are
    /// preserved verbatim. Everything in between is replaced with a single
    /// summary message that retains key facts.
    ///
    /// This is intentionally a separate async step called *before*
    /// `build_request()` — it mutates `self.messages` in place so that
    /// `truncate_to_budget()` has a smaller history to work with.
    pub async fn compact_if_needed(&mut self, provider: &impl Provider) -> Result<(), AgentError> {
        let total_tokens: u32 = self.messages.iter().map(|m| m.estimate_tokens()).sum();
        let threshold = self.config.compact_budget();

        if total_tokens <= threshold {
            return Ok(());
        }

        // Need at least 4 messages to compact (first + 2 middle + 1 recent)
        if self.messages.len() < 4 {
            return Ok(());
        }

        // Keep the first message (task context) and the last ~10 messages.
        // Everything in between becomes the compaction candidate.
        let keep_recent = 10.min(self.messages.len() - 1);
        let compact_end = self.messages.len() - keep_recent;

        // Nothing to compact if the boundary is at or before index 1
        if compact_end <= 1 {
            return Ok(());
        }

        let summary = self.summarize_conversation(provider, 1, compact_end).await?;

        // Rebuild messages: [first] + [summary] + [recent tail]
        let first = self.messages[0].clone();
        let tail: Vec<Message> = self.messages[compact_end..].to_vec();

        self.messages.clear();
        self.messages.push(first);
        self.messages.push(Message::user(format!(
            "[Conversation Summary — compaction #{}]\n{}",
            self.compaction_count + 1,
            summary,
        )));
        // Add an assistant acknowledgment so the next message can be any role
        self.messages.push(Message::assistant(
            "Understood, I have the conversation summary and will continue from here.",
        ));
        self.messages.extend(tail);

        self.compaction_count += 1;

        tracing::info!(
            session_id = %self.id,
            compaction_count = self.compaction_count,
            messages_before = compact_end - 1,
            messages_after = self.messages.len(),
            "compacted conversation history via summarization"
        );

        Ok(())
    }

    /// Summarize a range of messages (exclusive of `start` and `end` indices)
    /// by asking the LLM to produce a concise summary.
    async fn summarize_conversation(
        &self,
        provider: &impl Provider,
        start: usize,
        end: usize,
    ) -> Result<String, AgentError> {
        let mut conversation_text = String::new();
        for msg in &self.messages[start..end] {
            let role_label = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::System => "System",
            };
            let text = msg.text();
            if !text.is_empty() {
                conversation_text.push_str(&format!("{}: {}\n\n", role_label, text));
            }
            // Note tool use/results without full content to keep the summary prompt small
            for content in &msg.content {
                match content {
                    Content::ToolUse { name, .. } => {
                        conversation_text.push_str(&format!("[Tool call: {}]\n", name));
                    }
                    Content::ToolResult { is_error, .. } => {
                        let status = if *is_error { "error" } else { "success" };
                        conversation_text.push_str(&format!("[Tool result: {}]\n", status));
                    }
                    _ => {}
                }
            }
        }

        let system = "You are a conversation summarizer. Produce a concise summary of the \
            conversation that preserves:\n\
            - Key decisions and their reasoning\n\
            - Important facts, file paths, and code references\n\
            - Current task state and what has been accomplished\n\
            - Any errors encountered and how they were resolved\n\
            - Outstanding action items\n\n\
            Be factual and concise. Do not add commentary or interpretation.";

        let prompt = format!(
            "Summarize this conversation segment:\n\n{}",
            conversation_text
        );

        // Use a small max_tokens for the summary — it should be concise.
        let config = CompletionConfig::new(self.config.model.clone())
            .with_max_tokens(2048);

        let request = CompletionRequest::new(config, vec![Message::user(prompt)])
            .with_system(system.to_string());

        let response = provider.complete(request).await?;
        Ok(response.text())
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

    /// Set the output directory for persisting large tool results.
    pub fn set_output_dir(&mut self, dir: impl Into<PathBuf>) {
        self.output_dir = Some(dir.into());
    }

    /// Apply tool results and prepare for the next turn.
    ///
    /// Stores results in `pending_tool_results` so that `build_request()`
    /// emits them as proper `Content::ToolResult` blocks (required by the
    /// Anthropic API), rather than plain text.
    ///
    /// When `output_dir` is set, large tool results are persisted to disk
    /// and replaced with a preview + file path, based on each tool's
    /// `max_result_size` setting.
    pub fn apply_tool_results(&mut self, results: Vec<ToolResult>) {
        let results = if let Some(ref output_dir) = self.output_dir {
            tool_result_budget::budget_tool_results(
                results,
                &self.pending_tool_calls,
                &self.tools,
                output_dir,
            )
        } else {
            results
        };
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
        self.session.compact_if_needed(self.provider.as_ref()).await?;
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
        self.session.compact_if_needed(self.provider.as_ref()).await?;
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
        self.session.compact_if_needed(self.provider.as_ref()).await?;
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

    // -- Compaction tests --

    /// A mock provider that returns a fixed summary for compaction tests.
    struct MockSummaryProvider {
        summary: String,
    }

    impl MockSummaryProvider {
        fn new(summary: impl Into<String>) -> Self {
            Self { summary: summary.into() }
        }
    }

    impl Provider for MockSummaryProvider {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[&str] { &["mock-model"] }
        async fn complete(&self, _request: CompletionRequest) -> Result<Response, AgentError> {
            Ok(Response {
                content: vec![Content::text(&self.summary)],
                tool_calls: vec![],
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(Usage { input_tokens: 10, output_tokens: 10 }),
            })
        }
        async fn complete_streaming(
            &self,
            request: CompletionRequest,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<crate::provider::StreamChunk>, AgentError> {
            let response = self.complete(request).await?;
            Ok(crate::provider::streaming_from_complete(response))
        }
    }

    #[tokio::test]
    async fn test_compact_if_needed_under_threshold_no_change() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(200_000)
            .with_compact_threshold(0.85);
        let mut session = Session::new(config);
        session.add_user_message("hello");
        session.add_assistant_message("hi there");

        let provider = MockSummaryProvider::new("summary");
        let msg_count_before = session.messages.len();
        session.compact_if_needed(&provider).await.unwrap();
        assert_eq!(session.messages.len(), msg_count_before);
        assert_eq!(session.compaction_count, 0);
    }

    #[tokio::test]
    async fn test_compact_if_needed_over_threshold_summarizes() {
        // Use a tiny context window so our messages exceed 85% threshold
        let config = CompletionConfig::new("test-model")
            .with_context_window(200)
            .with_max_tokens(20)
            .with_compact_threshold(0.85);
        let mut session = Session::new(config);

        // First message (task context, always preserved)
        session.add_user_message("Implement feature X: a complex feature requiring many steps");
        // Add enough middle messages to exceed threshold
        for i in 0..20 {
            session.add_assistant_message(&format!(
                "Working on step {} with detailed explanation of changes", i
            ));
            session.add_user_message(&format!("Continue with step {}", i + 1));
        }
        let original_count = session.messages.len();

        let provider = MockSummaryProvider::new("Summary: implemented steps 0-19 of feature X");
        session.compact_if_needed(&provider).await.unwrap();

        // Should have compacted: first msg + summary + ack + ~10 recent
        assert!(session.messages.len() < original_count);
        assert_eq!(session.compaction_count, 1);
        // First message preserved
        assert_eq!(session.messages[0].text(), "Implement feature X: a complex feature requiring many steps");
        // Second message is the summary
        assert!(session.messages[1].text().contains("Conversation Summary"));
        assert!(session.messages[1].text().contains("implemented steps 0-19"));
        // Third message is the ack
        assert_eq!(session.messages[2].role, Role::Assistant);
        // Recent messages preserved at the tail
        let last = session.messages.last().unwrap();
        assert!(last.text().contains("Continue with step"));
    }

    #[tokio::test]
    async fn test_compact_if_needed_too_few_messages() {
        // Even with a tiny window, < 4 messages should not compact
        let config = CompletionConfig::new("test-model")
            .with_context_window(10)
            .with_compact_threshold(0.5);
        let mut session = Session::new(config);
        session.add_user_message("hello world this is a long message that exceeds budget");
        session.add_assistant_message("response");
        session.add_user_message("follow up");

        let provider = MockSummaryProvider::new("summary");
        session.compact_if_needed(&provider).await.unwrap();
        assert_eq!(session.compaction_count, 0);
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn test_compact_budget() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(200_000)
            .with_compact_threshold(0.85);
        assert_eq!(config.compact_budget(), 170_000);

        let config2 = CompletionConfig::new("test-model")
            .with_context_window(100)
            .with_compact_threshold(0.5);
        assert_eq!(config2.compact_budget(), 50);
    }
}
