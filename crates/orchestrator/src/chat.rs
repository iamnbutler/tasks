//! Orchestrator chat — conversational interface with users.
//!
//! The orchestrator chat allows users to have a conversation with the orchestrator,
//! asking questions about system state, requesting actions (create tasks, change mode),
//! and receiving updates about major events.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tracing::info;

use models::project::Project;
use models::task::{Task, TaskState};
use tasks_agent::{
    AnthropicProvider, CompletionConfig, CompletionRequest, Message, Provider,
};

use crate::error::OrchestratorError;

/// Default model for orchestrator chat.
const DEFAULT_CHAT_MODEL: &str = "claude-opus-4-6";

/// Convert TaskState to a string representation.
fn task_state_str(state: &TaskState) -> &'static str {
    match state {
        TaskState::Waiting => "waiting",
        TaskState::Blocked => "blocked",
        TaskState::Running => "running",
        TaskState::Question => "question",
        TaskState::Testing => "testing",
        TaskState::AwaitingMerge => "awaiting_merge",
        TaskState::Conflict => "conflict",
        TaskState::ChangesRequested => "changes_requested",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled",
    }
}

/// Default maximum tokens for chat response.
const DEFAULT_CHAT_MAX_TOKENS: u32 = 32_000;

/// Context for orchestrator chat — snapshot of current system state.
#[derive(Debug, Clone)]
pub struct ChatContext {
    /// Current operating mode.
    pub mode: String,
    /// All projects.
    pub projects: Vec<Project>,
    /// All tasks with their current state.
    pub tasks: Vec<Task>,
    /// Recent orchestrator events (last N).
    pub recent_events: Vec<ChatEvent>,
    /// Whether a human is currently connected.
    pub human_present: bool,
}

/// A simplified event for chat context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatEvent {
    pub event_type: String,
    pub timestamp: String,
    pub summary: String,
}

/// An action the orchestrator chat wants the system to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ChatAction {
    /// Set a task's dispatch priority. Lower numbers are higher priority.
    SetTaskPriority {
        task_id: String,
        priority: i32,
        reason: String,
    },
}

/// Result of a chat interaction.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The orchestrator's response text.
    pub message: String,
    /// Structured actions to execute (e.g., priority overrides from human direction).
    pub actions: Vec<ChatAction>,
}

/// Handler for orchestrator chat interactions.
pub struct OrchestratorChat {
    provider: AnthropicProvider,
    model: String,
    max_tokens: u32,
}

impl OrchestratorChat {
    /// Create a new orchestrator chat handler.
    pub fn new(provider: AnthropicProvider) -> Self {
        Self {
            provider,
            model: DEFAULT_CHAT_MODEL.to_string(),
            max_tokens: DEFAULT_CHAT_MAX_TOKENS,
        }
    }

    /// Create from environment (ANTHROPIC_API_KEY).
    ///
    /// Optional env vars for LLM configuration:
    /// - `TASKS_CHAT_MODEL` — model name (default: `claude-opus-4-6`)
    /// - `TASKS_CHAT_MAX_TOKENS` — max response tokens (default: 32000)
    pub fn from_env() -> Result<Self, OrchestratorError> {
        let provider = AnthropicProvider::from_env().map_err(OrchestratorError::Agent)?;
        let mut instance = Self::new(provider);

        if let Ok(model) = std::env::var("TASKS_CHAT_MODEL") {
            instance.model = model;
        }
        if let Ok(max_tokens) = std::env::var("TASKS_CHAT_MAX_TOKENS") {
            if let Ok(val) = max_tokens.parse::<u32>() {
                instance.max_tokens = val;
            }
        }

        Ok(instance)
    }

    /// Set a custom model for chat.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set custom max tokens for chat responses.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Process a human message and generate a response.
    ///
    /// This is the main entry point for chat interactions. It:
    /// 1. Builds context from the current system state
    /// 2. Constructs a prompt with the conversation history
    /// 3. Calls the LLM for a response
    /// 4. Handles any tool calls (actions requested)
    /// 5. Returns the final response
    pub async fn process_message(
        &self,
        message: &str,
        context: &ChatContext,
        conversation_history: &[Message],
    ) -> Result<ChatResponse, OrchestratorError> {
        info!(message_len = message.len(), "Processing orchestrator chat message");

        let system_prompt = self.build_system_prompt(context);

        // Build messages: history + new user message
        let mut messages = conversation_history.to_vec();
        messages.push(Message::user(message));

        let config = CompletionConfig::new(&self.model).with_max_tokens(self.max_tokens);
        let request = CompletionRequest::new(config, messages)
            .with_system(system_prompt);

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(OrchestratorError::Agent)?;

        let response_text = response.text();
        info!(response_len = response_text.len(), "Chat response generated");

        // Parse structured actions from the response (if any)
        let (message, actions) = parse_chat_actions(&response_text, &context.tasks);

        Ok(ChatResponse { message, actions })
    }

    /// Build the system prompt with current context.
    fn build_system_prompt(&self, context: &ChatContext) -> String {
        let tasks_summary = self.summarize_tasks(&context.tasks);
        let projects_summary = self.summarize_projects(&context.projects);
        let events_summary = self.summarize_events(&context.recent_events);

        format!(
            r#"You are the orchestrator for the Tasks platform — an AI project foreman that coordinates coding agents working on GitHub issues.

## Your Role
- You help users understand system status and make decisions
- You can explain system status and help users make decisions
- You provide updates on merges, conflicts, and agent progress
- You are helpful, concise, and action-oriented

## Current System State

**Operating Mode:** {mode}
**Human Present:** {human_present}

### Projects
{projects_summary}

### Tasks Summary
{tasks_summary}

### Recent Activity
{events_summary}

## Priority Management
You can change task dispatch priority when the user asks. To do so, include an
ACTION block at the end of your response (after your conversational reply):

```action
{{"action": "set_task_priority", "task_id": "<full-task-id>", "priority": <number>, "reason": "<why>"}}
```

Priority rules:
- Lower numbers = higher priority (dispatched first)
- Use priority 1-10 for urgent human-directed overrides
- Use priority 50 for moderate bumps
- Default computed priority is around 100
- Multiple ACTION blocks are allowed (one per line inside the block)

Only include ACTION blocks when the user explicitly asks to change priority,
reorder tasks, or focus on specific work. Do NOT include them for status queries.

## Guidelines
- Be concise but informative
- Proactively mention relevant system state when helpful
- When asked to prioritize or reorder tasks, use ACTION blocks to make it happen
- Reference tasks by their short ID (first 8 chars) in conversation but use full IDs in ACTION blocks"#,
            mode = context.mode,
            human_present = if context.human_present { "Yes" } else { "No" },
            projects_summary = projects_summary,
            tasks_summary = tasks_summary,
            events_summary = events_summary,
        )
    }

    /// Summarize tasks for context.
    fn summarize_tasks(&self, tasks: &[Task]) -> String {
        if tasks.is_empty() {
            return "No tasks.".to_string();
        }

        let mut by_state: BTreeMap<&'static str, Vec<&Task>> = BTreeMap::new();
        for task in tasks {
            let state = task_state_str(&task.state);
            by_state.entry(state).or_default().push(task);
        }

        let mut lines = Vec::new();
        for (state, tasks) in &by_state {
            lines.push(format!("- **{}**: {} task(s)", state, tasks.len()));
            // Show up to 5 tasks per state with full IDs for action targeting
            for task in tasks.iter().take(5) {
                let priority_str = task
                    .priority
                    .map(|p| format!(" [pri={}]", p))
                    .unwrap_or_default();
                lines.push(format!(
                    "  - `{}` ({}): {}{}",
                    task.id,
                    &task.id[..8.min(task.id.len())],
                    task.title,
                    priority_str,
                ));
            }
            if tasks.len() > 5 {
                lines.push(format!("  - ... and {} more", tasks.len() - 5));
            }
        }

        lines.join("\n")
    }

    /// Summarize projects for context.
    fn summarize_projects(&self, projects: &[Project]) -> String {
        if projects.is_empty() {
            return "No projects configured.".to_string();
        }

        projects
            .iter()
            .map(|p| format!("- {}", p.repo))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Summarize recent events for context.
    fn summarize_events(&self, events: &[ChatEvent]) -> String {
        if events.is_empty() {
            return "No recent activity.".to_string();
        }

        events
            .iter()
            .take(10)
            .map(|e| format!("- [{}] {}", e.event_type, e.summary))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Convert an event to a chat event summary.
pub fn event_to_chat_event(event_type: &str, data: &serde_json::Value, timestamp: &str) -> ChatEvent {
    let summary = match event_type {
        "orchestrator:decision" => {
            let approved = data.get("approved").and_then(|v| v.as_bool()).unwrap_or(false);
            let task_id = data.get("task_id").and_then(|v| v.as_str()).unwrap_or("unknown");
            if approved {
                format!("Approved merge for task {}", task_id)
            } else {
                format!("Rejected merge for task {}", task_id)
            }
        }
        "orchestrator:feedback" => {
            let task_id = data.get("task_id").and_then(|v| v.as_str()).unwrap_or("unknown");
            format!("Sent feedback to task {}", task_id)
        }
        "orchestrator:escalation" => {
            let action = data.get("action").and_then(|v| v.as_str()).unwrap_or("escalation");
            let reason = data.get("reason").and_then(|v| v.as_str()).unwrap_or("unknown reason");
            format!("{}: {}", action, reason)
        }
        "merge:completed" => {
            let task_id = data.get("task_id").and_then(|v| v.as_str()).unwrap_or("unknown");
            format!("Merge completed for task {}", task_id)
        }
        "merge:conflict" => {
            let task_id = data.get("task_id").and_then(|v| v.as_str()).unwrap_or("unknown");
            format!("Merge conflict detected for task {}", task_id)
        }
        "task:state:completed" => "Task completed".to_string(),
        "task:state:failed" => "Task failed".to_string(),
        "task:state:running" => "Task started running".to_string(),
        "system:mode:play" => "Mode changed to Play".to_string(),
        "system:mode:pause" => "Mode changed to Pause".to_string(),
        "system:mode:stop" => "Mode changed to Stop".to_string(),
        _ => format!("Event: {}", event_type),
    };

    ChatEvent {
        event_type: event_type.to_string(),
        timestamp: timestamp.to_string(),
        summary,
    }
}

/// Parse structured actions from an LLM chat response.
///
/// Looks for an ```action code block at the end of the response. Each line
/// inside the block is parsed as a JSON `ChatAction`. The conversational
/// text before the block is returned as the message.
///
/// `tasks` is used to validate that referenced task IDs actually exist.
fn parse_chat_actions(response: &str, tasks: &[Task]) -> (String, Vec<ChatAction>) {
    let task_ids: std::collections::HashSet<&str> =
        tasks.iter().map(|t| t.id.as_str()).collect();

    // Look for ```action block
    let action_marker = "```action";
    let Some(block_start) = response.find(action_marker) else {
        return (response.to_string(), Vec::new());
    };

    let message = response[..block_start].trim().to_string();
    let after_marker = block_start + action_marker.len();

    // Find closing ```
    let block_content = if let Some(end) = response[after_marker..].find("```") {
        &response[after_marker..after_marker + end]
    } else {
        &response[after_marker..]
    };

    let mut actions = Vec::new();
    for line in block_content.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        match serde_json::from_str::<ChatAction>(line) {
            Ok(action) => {
                // Validate task ID exists
                let valid = match &action {
                    ChatAction::SetTaskPriority { task_id, .. } => {
                        task_ids.contains(task_id.as_str())
                    }
                };
                if valid {
                    actions.push(action);
                } else {
                    info!("Ignoring chat action with unknown task ID: {}", line);
                }
            }
            Err(e) => {
                info!("Failed to parse chat action: {} — {}", line, e);
            }
        }
    }

    (message, actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_event_to_chat_event_decision_approved() {
        let data = json!({
            "approved": true,
            "task_id": "abc123"
        });
        let event = event_to_chat_event("orchestrator:decision", &data, "2026-01-01T00:00:00Z");
        assert_eq!(event.event_type, "orchestrator:decision");
        assert!(event.summary.contains("Approved"));
        assert!(event.summary.contains("abc123"));
    }

    #[test]
    fn test_event_to_chat_event_decision_rejected() {
        let data = json!({
            "approved": false,
            "task_id": "def456"
        });
        let event = event_to_chat_event("orchestrator:decision", &data, "2026-01-01T00:00:00Z");
        assert!(event.summary.contains("Rejected"));
    }

    #[test]
    fn test_event_to_chat_event_mode_change() {
        let event = event_to_chat_event("system:mode:play", &serde_json::json!({}), "2026-01-01T00:00:00Z");
        assert!(event.summary.contains("Play"));
    }

    #[test]
    fn test_summarize_empty_tasks() {
        let chat = OrchestratorChat::new(AnthropicProvider::new("test"));
        let summary = chat.summarize_tasks(&[]);
        assert_eq!(summary, "No tasks.");
    }

    #[test]
    fn test_summarize_empty_projects() {
        let chat = OrchestratorChat::new(AnthropicProvider::new("test"));
        let summary = chat.summarize_projects(&[]);
        assert_eq!(summary, "No projects configured.");
    }

    #[test]
    fn test_summarize_empty_events() {
        let chat = OrchestratorChat::new(AnthropicProvider::new("test"));
        let summary = chat.summarize_events(&[]);
        assert_eq!(summary, "No recent activity.");
    }

    // --- Chat action parsing tests ---

    use models::task::TaskSource;

    fn make_task(id: &str) -> Task {
        Task::new(id.to_string(), TaskSource::Internal, id, "proj".to_string())
    }

    #[test]
    fn test_parse_chat_actions_no_block() {
        let tasks = vec![make_task("t1")];
        let (message, actions) = parse_chat_actions("Just a normal response.", &tasks);
        assert_eq!(message, "Just a normal response.");
        assert!(actions.is_empty());
    }

    #[test]
    fn test_parse_chat_actions_with_priority() {
        let tasks = vec![make_task("task-abc-123")];
        let response = r#"I'll prioritize that task for you.

```action
{"action": "set_task_priority", "task_id": "task-abc-123", "priority": 1, "reason": "human requested"}
```"#;
        let (message, actions) = parse_chat_actions(response, &tasks);
        assert_eq!(message, "I'll prioritize that task for you.");
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ChatAction::SetTaskPriority {
                task_id,
                priority,
                reason,
            } => {
                assert_eq!(task_id, "task-abc-123");
                assert_eq!(*priority, 1);
                assert_eq!(reason, "human requested");
            }
        }
    }

    #[test]
    fn test_parse_chat_actions_multiple() {
        let tasks = vec![make_task("t1"), make_task("t2")];
        let response = r#"Done.

```action
{"action": "set_task_priority", "task_id": "t1", "priority": 1, "reason": "urgent"}
{"action": "set_task_priority", "task_id": "t2", "priority": 2, "reason": "next"}
```"#;
        let (_, actions) = parse_chat_actions(response, &tasks);
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn test_parse_chat_actions_unknown_task_id_rejected() {
        let tasks = vec![make_task("t1")];
        let response = r#"Done.

```action
{"action": "set_task_priority", "task_id": "nonexistent", "priority": 1, "reason": "test"}
```"#;
        let (_, actions) = parse_chat_actions(response, &tasks);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_parse_chat_actions_malformed_json_skipped() {
        let tasks = vec![make_task("t1")];
        let response = r#"Done.

```action
{not valid json}
{"action": "set_task_priority", "task_id": "t1", "priority": 5, "reason": "ok"}
```"#;
        let (_, actions) = parse_chat_actions(response, &tasks);
        assert_eq!(actions.len(), 1);
    }
}
