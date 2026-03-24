//! Automation executor — runs automation prompts via the LLM.
//!
//! Provides lightweight execution of automation prompts without the
//! full container session infrastructure used for tasks. This is
//! appropriate for automations that don't need to modify code directly.

use std::fmt::Write;

use tracing::{info, warn};

use tasks_agent::{
    AnthropicProvider, CompletionConfig, CompletionRequest, Message, Provider, Response,
};

use crate::model::automation::Automation;
use crate::model::project::Project;

/// Default model for automation execution.
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Maximum tokens for automation response.
const MAX_TOKENS: u32 = 8192;

/// Error type for automation execution.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("provider error: {0}")]
    Provider(#[from] tasks_agent::AgentError),
    #[error("execution error: {0}")]
    Execution(String),
}

/// Result of an automation execution.
#[derive(Debug)]
pub struct ExecutionResult {
    /// The output text from the LLM.
    pub output: String,
    /// Token usage from the execution.
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Context provided to the automation during execution.
#[derive(Debug, Default)]
pub struct ExecutionContext {
    /// Recent activity summary (for trend analysis automations).
    pub recent_activity: Option<String>,
    /// Previous run output (for comparison automations).
    pub previous_output: Option<String>,
    /// Additional context provided by the caller.
    pub additional_context: Option<String>,
}

/// Executor for running automations via the LLM.
pub struct AutomationExecutor {
    provider: AnthropicProvider,
    model: String,
}

impl AutomationExecutor {
    /// Create a new executor with the given provider.
    pub fn new(provider: AnthropicProvider) -> Self {
        Self {
            provider,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Create an executor from environment variables (ANTHROPIC_API_KEY).
    pub fn from_env() -> Result<Self, ExecutionError> {
        let provider =
            AnthropicProvider::from_env().map_err(ExecutionError::Provider)?;
        Ok(Self::new(provider))
    }

    /// Set a custom model for execution.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Execute an automation and return the result.
    pub async fn execute(
        &self,
        automation: &Automation,
        project: &Project,
        context: &ExecutionContext,
    ) -> Result<ExecutionResult, ExecutionError> {
        info!(
            automation_id = %automation.id,
            automation_name = %automation.name,
            project = %project.repo,
            "Executing automation"
        );

        let system = build_system_prompt(project);
        let user_prompt = build_user_prompt(automation, context);

        let config = CompletionConfig::new(&self.model).with_max_tokens(MAX_TOKENS);
        let request = CompletionRequest::new(config, vec![Message::user(user_prompt)])
            .with_system(system);

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(ExecutionError::Provider)?;

        let output = response.text();
        let (input_tokens, output_tokens) = extract_usage(&response);

        info!(
            automation_id = %automation.id,
            output_len = output.len(),
            input_tokens = input_tokens,
            output_tokens = output_tokens,
            "Automation execution complete"
        );

        Ok(ExecutionResult {
            output,
            input_tokens,
            output_tokens,
        })
    }

    /// Execute an automation with streaming, calling the callback for each chunk.
    ///
    /// This allows for real-time output updates during long-running automations.
    pub async fn execute_streaming<F>(
        &self,
        automation: &Automation,
        project: &Project,
        context: &ExecutionContext,
        mut on_chunk: F,
    ) -> Result<ExecutionResult, ExecutionError>
    where
        F: FnMut(&str),
    {
        info!(
            automation_id = %automation.id,
            automation_name = %automation.name,
            project = %project.repo,
            "Executing automation (streaming)"
        );

        let system = build_system_prompt(project);
        let user_prompt = build_user_prompt(automation, context);

        let config = CompletionConfig::new(&self.model).with_max_tokens(MAX_TOKENS);
        let request = CompletionRequest::new(config, vec![Message::user(user_prompt)])
            .with_system(system);

        let mut rx = self
            .provider
            .complete_streaming(request)
            .await
            .map_err(ExecutionError::Provider)?;

        let mut output = String::new();
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;

        while let Some(chunk) = rx.recv().await {
            match chunk {
                tasks_agent::StreamChunk::Text(text) => {
                    on_chunk(&text);
                    output.push_str(&text);
                }
                tasks_agent::StreamChunk::Thinking(text) => {
                    // Include thinking in callback but not in final output
                    on_chunk(&format!("[thinking] {}", text));
                }
                tasks_agent::StreamChunk::Complete(response) => {
                    let (i, o) = extract_usage(&response);
                    input_tokens = i;
                    output_tokens = o;
                }
                tasks_agent::StreamChunk::Error(e) => {
                    warn!(error = %e, "Streaming error during automation execution");
                    return Err(ExecutionError::Execution(e));
                }
                _ => {}
            }
        }

        info!(
            automation_id = %automation.id,
            output_len = output.len(),
            input_tokens = input_tokens,
            output_tokens = output_tokens,
            "Automation streaming execution complete"
        );

        Ok(ExecutionResult {
            output,
            input_tokens,
            output_tokens,
        })
    }
}

/// Build the system prompt for automation execution.
fn build_system_prompt(project: &Project) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "You are an automation assistant for the {} project.",
        project.repo
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Your role is to execute automated tasks and provide clear, actionable output."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Guidelines:").unwrap();
    writeln!(out, "- Be concise and focused on the task at hand").unwrap();
    writeln!(out, "- Provide structured output when appropriate (use markdown)").unwrap();
    writeln!(out, "- If you identify issues or recommendations, list them clearly").unwrap();
    writeln!(
        out,
        "- If the task cannot be completed, explain why and suggest alternatives"
    )
    .unwrap();

    out
}

/// Build the user prompt for automation execution.
fn build_user_prompt(automation: &Automation, context: &ExecutionContext) -> String {
    let mut out = String::new();

    writeln!(out, "# Automation: {}\n", automation.name).unwrap();
    writeln!(out, "{}\n", automation.prompt).unwrap();

    // Add context sections if provided
    if let Some(ref recent) = context.recent_activity {
        writeln!(out, "## Recent Activity\n").unwrap();
        writeln!(out, "{}\n", recent).unwrap();
    }

    if let Some(ref previous) = context.previous_output {
        writeln!(out, "## Previous Run Output\n").unwrap();
        writeln!(out, "{}\n", previous).unwrap();
    }

    if let Some(ref additional) = context.additional_context {
        writeln!(out, "## Additional Context\n").unwrap();
        writeln!(out, "{}\n", additional).unwrap();
    }

    out
}

/// Extract token usage from a response.
fn extract_usage(response: &Response) -> (u32, u32) {
    response
        .usage
        .as_ref()
        .map(|u| (u.input_tokens, u.output_tokens))
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::automation::TriggerType;

    fn test_automation() -> Automation {
        Automation::new(
            "auto-1",
            "proj-1",
            "Test Automation",
            "Check the documentation for outdated references.",
            TriggerType::Manual,
        )
    }

    fn test_project() -> Project {
        Project::new("proj-1", "acme/widgets")
    }

    #[test]
    fn test_build_system_prompt() {
        let project = test_project();
        let prompt = build_system_prompt(&project);

        assert!(prompt.contains("acme/widgets"));
        assert!(prompt.contains("automation assistant"));
        assert!(prompt.contains("Guidelines"));
    }

    #[test]
    fn test_build_user_prompt_basic() {
        let automation = test_automation();
        let context = ExecutionContext::default();
        let prompt = build_user_prompt(&automation, &context);

        assert!(prompt.contains("# Automation: Test Automation"));
        assert!(prompt.contains("Check the documentation for outdated references."));
        // No context sections
        assert!(!prompt.contains("## Recent Activity"));
        assert!(!prompt.contains("## Previous Run Output"));
    }

    #[test]
    fn test_build_user_prompt_with_context() {
        let automation = test_automation();
        let context = ExecutionContext {
            recent_activity: Some("3 PRs merged, 5 issues closed".to_string()),
            previous_output: Some("All documentation is up to date.".to_string()),
            additional_context: Some("Focus on API docs".to_string()),
        };
        let prompt = build_user_prompt(&automation, &context);

        assert!(prompt.contains("## Recent Activity"));
        assert!(prompt.contains("3 PRs merged"));
        assert!(prompt.contains("## Previous Run Output"));
        assert!(prompt.contains("All documentation is up to date."));
        assert!(prompt.contains("## Additional Context"));
        assert!(prompt.contains("Focus on API docs"));
    }
}
