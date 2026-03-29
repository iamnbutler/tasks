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

/// Number of recent messages always kept verbatim (ground truth anchor).
const RECENT_VERBATIM_COUNT: usize = 6;

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
    /// Actual input token count from the last API response, used to calibrate
    /// the chars/4 heuristic for more accurate budget estimation.
    last_input_tokens: Option<u32>,
    /// Number of messages that were sent in the last request (used with
    /// `last_input_tokens` to compute a correction factor).
    last_request_estimated_tokens: Option<u32>,
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
            last_input_tokens: None,
            last_request_estimated_tokens: None,
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

    /// Compute a correction factor for the chars/4 heuristic using actual API
    /// token counts from the previous turn. Returns 1.0 if no data is available.
    fn token_correction_factor(&self) -> f32 {
        match (self.last_input_tokens, self.last_request_estimated_tokens) {
            (Some(actual), Some(estimated)) if estimated > 0 => {
                let factor = actual as f32 / estimated as f32;
                // Clamp to avoid wild swings from outliers
                factor.clamp(0.5, 2.0)
            }
            _ => 1.0,
        }
    }

    /// Score messages by retention priority. Lower scores are dropped first.
    ///
    /// Detects superseded tool results: when a later tool call targets the same
    /// resource as an earlier one (e.g., reading the same file twice), the earlier
    /// tool call and its result get a reduced score.
    fn score_messages(&self) -> Vec<f32> {
        let total = self.messages.len();
        let mut scores = vec![1.0_f32; total];

        if total == 0 {
            return scores;
        }

        // First message (task context) is always pinned
        scores[0] = f32::MAX;

        // Pin the last RECENT_VERBATIM_COUNT messages as ground truth
        let pin_start = total.saturating_sub(RECENT_VERBATIM_COUNT);
        for score in scores.iter_mut().skip(pin_start) {
            *score = f32::MAX;
        }

        // Build a map of (tool_name, resource_key) -> last message index containing that tool use.
        // Then mark earlier uses of the same resource as superseded (lower priority).
        let mut resource_last_use: HashMap<(String, String), usize> = HashMap::new();

        for (i, msg) in self.messages.iter().enumerate() {
            if msg.role != Role::Assistant {
                continue;
            }
            for content in &msg.content {
                if let Content::ToolUse { name, input, .. } = content {
                    if let Some(key) = Self::extract_resource_key(name, input) {
                        resource_last_use.insert((name.clone(), key), i);
                    }
                }
            }
        }

        // Mark earlier (superseded) tool calls and their paired results
        for (i, msg) in self.messages.iter().enumerate() {
            if msg.role != Role::Assistant || scores[i] == f32::MAX {
                continue;
            }
            for content in &msg.content {
                if let Content::ToolUse { name, input, .. } = content {
                    if let Some(key) = Self::extract_resource_key(name, input) {
                        if let Some(&last_idx) = resource_last_use.get(&(name.clone(), key)) {
                            if last_idx > i {
                                // This tool call was superseded by a later one
                                scores[i] = scores[i].min(0.3);
                                // Also reduce score of the paired ToolResult (next message)
                                if i + 1 < total && scores[i + 1] != f32::MAX {
                                    let next = &self.messages[i + 1];
                                    if next.role == Role::User
                                        && next.content.iter().all(|c| matches!(c, Content::ToolResult { .. }))
                                    {
                                        scores[i + 1] = scores[i + 1].min(0.3);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        scores
    }

    /// Extract a resource key from tool arguments for superseding detection.
    /// Returns None if the tool doesn't have a recognizable resource identifier.
    fn extract_resource_key(tool_name: &str, input: &serde_json::Value) -> Option<String> {
        // Common patterns for file/resource-based tools
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            return Some(path.to_string());
        }
        if let Some(file) = input.get("file").and_then(|v| v.as_str()) {
            return Some(file.to_string());
        }
        if let Some(file_path) = input.get("file_path").and_then(|v| v.as_str()) {
            return Some(file_path.to_string());
        }
        if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
            return Some(url.to_string());
        }
        // For commands like bash, use the tool name + command as a very rough key.
        // Only exact command matches are treated as superseding.
        if tool_name == "bash" || tool_name == "execute" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                return Some(cmd.to_string());
            }
        }
        None
    }

    /// Generate a deterministic summary of dropped messages.
    ///
    /// Produces factual bullet points (not narrative) so the model knows what
    /// context was lost and can ask clarifying questions if needed.
    fn summarize_dropped(dropped: &[&Message]) -> String {
        if dropped.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "[Context compacted: {} messages summarized]",
            dropped.len()
        ));

        for msg in dropped {
            match msg.role {
                Role::Assistant => {
                    let tool_uses: Vec<&str> = msg.content.iter().filter_map(|c| {
                        if let Content::ToolUse { name, .. } = c { Some(name.as_str()) } else { None }
                    }).collect();

                    if !tool_uses.is_empty() {
                        lines.push(format!("- Assistant called: {}", tool_uses.join(", ")));
                    } else {
                        let text = msg.text();
                        if !text.is_empty() {
                            let truncated: String = text.chars().take(100).collect();
                            if text.len() > 100 {
                                lines.push(format!("- Assistant: {}...", truncated));
                            } else {
                                lines.push(format!("- Assistant: {}", truncated));
                            }
                        }
                    }
                }
                Role::User => {
                    let is_tool_result = msg.content.iter().all(|c| matches!(c, Content::ToolResult { .. }));
                    if is_tool_result {
                        let count = msg.content.len();
                        let had_error = msg.content.iter().any(|c| {
                            matches!(c, Content::ToolResult { is_error: true, .. })
                        });
                        if had_error {
                            lines.push(format!("- Tool result ({} results, had errors)", count));
                        } else {
                            lines.push(format!("- Tool result ({} results)", count));
                        }
                    } else {
                        let text = msg.text();
                        if !text.is_empty() {
                            let truncated: String = text.chars().take(80).collect();
                            if text.len() > 80 {
                                lines.push(format!("- User: {}...", truncated));
                            } else {
                                lines.push(format!("- User: {}", truncated));
                            }
                        }
                    }
                }
                Role::System => {
                    lines.push("- System message".to_string());
                }
            }
        }

        lines.join("\n")
    }

    /// Truncate messages to fit within the context window budget.
    ///
    /// Strategy:
    /// 1. Score messages by importance (superseded tool results get lower scores).
    /// 2. Pin the first message (task context) and recent messages (ground truth).
    /// 3. Drop lowest-scored middle messages first until under budget.
    /// 4. Insert a deterministic summary of dropped messages after the first message.
    /// 5. Skip orphaned ToolResult messages at the truncation boundary.
    fn truncate_to_budget(&mut self) -> Vec<Message> {
        let budget = self.config.input_budget();
        let correction = self.token_correction_factor();

        // Account for system prompt and tool definitions
        let mut overhead: u32 = 0;
        if let Some(ref system) = self.system_prompt {
            overhead += (system.len() as u32) / 4 + 10;
        }
        for tool in &self.tools {
            overhead += (tool.description.len() as u32 + tool.parameters.to_string().len() as u32) / 4 + 20;
        }
        let overhead = (overhead as f32 * correction) as u32;

        let available = budget.saturating_sub(overhead);

        // Estimate total tokens with correction factor
        let raw_total: u32 = self.messages.iter().map(|m| m.estimate_tokens()).sum();
        let total_tokens = (raw_total as f32 * correction) as u32;

        // Fast path: skip truncation if under budget
        if total_tokens <= available {
            // Save estimated tokens for next calibration cycle
            self.last_request_estimated_tokens = Some(raw_total);
            return self.messages.clone();
        }

        let total = self.messages.len();
        if total <= 2 {
            self.last_request_estimated_tokens = Some(raw_total);
            return self.messages.clone();
        }

        // Score all messages
        let scores = self.score_messages();

        // Build list of droppable message indices sorted by priority.
        // First: non-pinned messages sorted by score ascending.
        // Fallback: if that's not enough, also include pinned middle messages
        // (indices 1..pin_start) sorted by their token estimate descending
        // to maximize space reclaimed per drop.
        let mut droppable: Vec<(usize, f32, u32)> = Vec::new();
        for i in 1..total {  // never drop index 0
            let tokens = (self.messages[i].estimate_tokens() as f32 * correction) as u32;
            if scores[i] < f32::MAX {
                droppable.push((i, scores[i], tokens));
            }
        }

        // Sort by score ascending (drop lowest priority first)
        droppable.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // If droppable messages aren't enough, allow dropping pinned messages
        // (everything except first and last message). Walk from oldest to newest
        // so the most recent messages survive longest.
        let droppable_tokens: u32 = droppable.iter().map(|&(_, _, t)| t).sum();
        if total_tokens.saturating_sub(droppable_tokens) > available {
            let mut pinned_fallback: Vec<(usize, f32, u32)> = Vec::new();
            for i in 1..total.saturating_sub(1) {
                if scores[i] == f32::MAX {
                    let tokens = (self.messages[i].estimate_tokens() as f32 * correction) as u32;
                    // Use a priority slightly above non-pinned (1.5) so they're dropped last
                    pinned_fallback.push((i, 1.5, tokens));
                }
            }
            // Already in oldest-first order, which is what we want for dropping
            droppable.extend(pinned_fallback);
        }

        // Drop messages until we're under budget
        let mut drop_set = std::collections::HashSet::new();
        let mut current_tokens = total_tokens;
        for &(idx, _score, tokens) in &droppable {
            if current_tokens <= available {
                break;
            }
            drop_set.insert(idx);
            current_tokens = current_tokens.saturating_sub(tokens);
        }

        if drop_set.is_empty() {
            self.last_request_estimated_tokens = Some(raw_total);
            return self.messages.clone();
        }

        // Collect dropped messages for summarization
        let dropped_msgs: Vec<&Message> = drop_set.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|i| &self.messages[i])
            .collect();

        // Generate summary
        let summary_text = Self::summarize_dropped(&dropped_msgs);
        let has_summary = !summary_text.is_empty();

        // Build result: first message, summary, then non-dropped messages
        let mut result = Vec::with_capacity(total - drop_set.len() + 2);
        result.push(self.messages[0].clone());

        if has_summary {
            result.push(Message::user(summary_text));
        }

        // Add kept messages (skip first, skip dropped)
        let mut i = 1;
        while i < total {
            if drop_set.contains(&i) {
                i += 1;
                continue;
            }

            // Check for orphaned ToolResult: a user ToolResult message
            // whose preceding assistant ToolUse was dropped
            let msg = &self.messages[i];
            let is_tool_result_only = msg.role == Role::User
                && msg.content.iter().all(|c| matches!(c, Content::ToolResult { .. }));

            if is_tool_result_only && i > 0 && drop_set.contains(&(i - 1)) {
                // The paired ToolUse was dropped, skip this orphan too
                i += 1;
                continue;
            }

            result.push(self.messages[i].clone());
            i += 1;
        }

        let dropped_count = total - (result.len() - if has_summary { 1 } else { 0 });
        let superseded_count = droppable.iter().filter(|(_, s, _)| *s <= 0.3).count();
        tracing::warn!(
            session_id = %self.id,
            total_messages = total,
            dropped = dropped_count,
            superseded = superseded_count,
            estimated_tokens = total_tokens,
            budget = available,
            correction_factor = %format!("{:.2}", correction),
            "compacted conversation history to fit context window"
        );

        // Save estimated tokens for calibration
        let result_tokens: u32 = result.iter().map(|m| m.estimate_tokens()).sum();
        self.last_request_estimated_tokens = Some(result_tokens);

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
            // Track actual input tokens for calibration
            self.last_input_tokens = Some(usage.input_tokens);
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
        // Should be truncated: first message + summary + recent messages
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
    fn test_truncation_inserts_summary() {
        let config = CompletionConfig::new("test-model")
            .with_context_window(100)
            .with_max_tokens(20);
        let mut session = Session::new(config);

        session.add_user_message("task: implement feature X");
        for i in 0..20 {
            session.add_assistant_message(&format!("working on step {} with lots of detail in the implementation", i));
            session.add_user_message(&format!("continue with step {}", i + 1));
        }

        let request = session.build_request();
        // Second message should be the summary (starts with [Context compacted:)
        assert!(request.messages.len() >= 2);
        let summary = &request.messages[1];
        assert_eq!(summary.role, Role::User);
        assert!(
            summary.text().starts_with("[Context compacted:"),
            "Expected summary message, got: {}",
            summary.text()
        );
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

        // No message after the first (and possible summary) should be an orphaned ToolResult
        for (idx, msg) in request.messages.iter().enumerate() {
            if idx == 0 { continue; }
            // Skip the summary message
            if msg.text().starts_with("[Context compacted:") { continue; }
            let is_tool_result_only = msg.role == Role::User
                && msg.content.iter().all(|c| matches!(c, Content::ToolResult { .. }));
            assert!(
                !is_tool_result_only,
                "tail contains orphaned ToolResult message at index {}",
                idx,
            );
        }
        // First message preserved
        assert_eq!(request.messages[0].text(), "implement feature X");
    }

    #[test]
    fn test_superseded_tool_results_dropped_first() {
        // Context window large enough to keep some but not all messages
        let config = CompletionConfig::new("test-model")
            .with_context_window(200)
            .with_max_tokens(20);
        let mut session = Session::new(config);

        // [0] User: task context
        session.add_user_message("implement feature X");

        // [1] First read of /tmp/foo.rs (will be superseded by [5])
        session.messages.push(Message::new(Role::Assistant, vec![
            Content::ToolUse {
                id: "tc-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/foo.rs"}),
            },
        ]));
        // [2] First read result
        session.messages.push(Message::new(Role::User, vec![
            Content::tool_result("tc-1", "original file contents that are quite long and take up tokens", false),
        ]));
        // [3] Normal conversation
        session.add_assistant_message("I see, let me edit the file");
        // [4] Normal conversation
        session.add_user_message("ok go ahead");
        // [5] Second read of the same file (supersedes [1])
        session.messages.push(Message::new(Role::Assistant, vec![
            Content::ToolUse {
                id: "tc-2".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/foo.rs"}),
            },
        ]));
        // [6] Second read result
        session.messages.push(Message::new(Role::User, vec![
            Content::tool_result("tc-2", "updated file contents", false),
        ]));
        // [7-10] Recent conversation
        session.add_assistant_message("file updated successfully");
        session.add_user_message("great");

        let scores = session.score_messages();

        // The first read (index 1) and its result (index 2) should have low scores
        assert!(scores[1] <= 0.3, "superseded tool call should have low score, got {}", scores[1]);
        assert!(scores[2] <= 0.3, "superseded tool result should have low score, got {}", scores[2]);

        // The second read (index 5) should not be penalized (it's the latest)
        // (It may be pinned by RECENT_VERBATIM_COUNT depending on total message count)
        assert!(scores[5] >= 1.0, "latest tool call should not be penalized, got {}", scores[5]);
    }

    #[test]
    fn test_correction_factor_defaults_to_one() {
        let session = Session::new(CompletionConfig::new("test-model"));
        assert_eq!(session.token_correction_factor(), 1.0);
    }

    #[test]
    fn test_correction_factor_from_usage() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        session.last_input_tokens = Some(200);
        session.last_request_estimated_tokens = Some(100);
        // Actual was 2x our estimate
        assert!((session.token_correction_factor() - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_correction_factor_clamped() {
        let mut session = Session::new(CompletionConfig::new("test-model"));
        // Wildly off estimate — should be clamped to 2.0
        session.last_input_tokens = Some(10000);
        session.last_request_estimated_tokens = Some(100);
        assert!((session.token_correction_factor() - 2.0).abs() < f32::EPSILON);

        // Very low actual — should be clamped to 0.5
        session.last_input_tokens = Some(1);
        session.last_request_estimated_tokens = Some(100);
        assert!((session.token_correction_factor() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_summarize_dropped_messages() {
        let msgs = vec![
            Message::assistant("I'll help you with that"),
            Message::new(Role::Assistant, vec![
                Content::ToolUse {
                    id: "tc-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "/tmp/foo.rs"}),
                },
            ]),
            Message::new(Role::User, vec![
                Content::tool_result("tc-1", "file contents", false),
            ]),
            Message::user("thanks, continue"),
        ];
        let refs: Vec<&Message> = msgs.iter().collect();
        let summary = Session::summarize_dropped(&refs);

        assert!(summary.contains("[Context compacted: 4 messages summarized]"));
        assert!(summary.contains("- Assistant: I'll help you with that"));
        assert!(summary.contains("- Assistant called: read_file"));
        assert!(summary.contains("- Tool result (1 results)"));
        assert!(summary.contains("- User: thanks, continue"));
    }

    #[test]
    fn test_cached_tokens_used_in_estimation() {
        let msg = Message::user("hello").with_tokens(42);
        assert_eq!(msg.estimate_tokens(), 42);

        // Without cached tokens, uses heuristic
        let msg = Message::user("hello");
        assert_ne!(msg.estimate_tokens(), 42);
    }

    #[test]
    fn test_extract_resource_key() {
        // path-based tool
        let key = Session::extract_resource_key(
            "read_file",
            &serde_json::json!({"path": "/tmp/foo.rs"}),
        );
        assert_eq!(key, Some("/tmp/foo.rs".to_string()));

        // file_path variant
        let key = Session::extract_resource_key(
            "edit",
            &serde_json::json!({"file_path": "/tmp/bar.rs"}),
        );
        assert_eq!(key, Some("/tmp/bar.rs".to_string()));

        // bash command
        let key = Session::extract_resource_key(
            "bash",
            &serde_json::json!({"command": "ls -la"}),
        );
        assert_eq!(key, Some("ls -la".to_string()));

        // no recognizable resource
        let key = Session::extract_resource_key(
            "think",
            &serde_json::json!({"text": "hmm"}),
        );
        assert_eq!(key, None);
    }
}
