//! Automation executor — runs automation prompts via the agent provider.
//!
//! When an automation run is triggered (manually or via scheduler), this module:
//! 1. Builds context for the automation (project info, automation details)
//! 2. Sends the prompt to the LLM via the agent provider
//! 3. Captures the output
//! 4. Updates the run status and emits completion/failure events

use std::sync::Arc;

use tasks_agent::{AnthropicProvider, CompletionConfig, CompletionRequest, Message, Provider};

use crate::model::automation::Automation;
use crate::model::project::Project;

/// Default model for automation execution.
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Maximum tokens for automation responses.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Result of executing an automation.
#[derive(Debug)]
pub struct ExecutionResult {
    /// The output from the agent (text response).
    pub output: Option<String>,
    /// Error message if execution failed.
    pub error: Option<String>,
    /// Whether execution succeeded.
    pub success: bool,
}

impl ExecutionResult {
    pub fn success(output: String) -> Self {
        Self {
            output: Some(output),
            error: None,
            success: true,
        }
    }

    pub fn failure(error: String) -> Self {
        Self {
            output: None,
            error: Some(error),
            success: false,
        }
    }
}

/// Executor for running automation prompts.
#[derive(Clone)]
pub struct AutomationExecutor {
    provider: Arc<AnthropicProvider>,
    model: String,
}

impl AutomationExecutor {
    /// Create a new executor with the given provider.
    pub fn new(provider: AnthropicProvider) -> Self {
        Self {
            provider: Arc::new(provider),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Create an executor from environment variables.
    pub fn from_env() -> Result<Self, tasks_agent::AgentError> {
        let provider = AnthropicProvider::from_env()?;
        Ok(Self::new(provider))
    }

    /// Set a custom model for execution.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Execute an automation and return the result.
    ///
    /// This builds the appropriate context and sends the prompt to the LLM.
    pub async fn execute(
        &self,
        automation: &Automation,
        project: Option<&Project>,
    ) -> ExecutionResult {
        // Build the system prompt with context
        let system_prompt = self.build_system_prompt(automation, project);

        // Build the user message with the automation prompt
        let user_message = self.build_user_prompt(automation);

        // Create the completion request
        let config = CompletionConfig::new(&self.model).with_max_tokens(DEFAULT_MAX_TOKENS);
        let messages = vec![Message::user(&user_message)];
        let request = CompletionRequest::new(config, messages).with_system(system_prompt);

        // Execute the request
        match self.provider.complete(request).await {
            Ok(response) => {
                let output = response.text();
                if output.is_empty() {
                    ExecutionResult::failure("Empty response from agent".to_string())
                } else {
                    ExecutionResult::success(output)
                }
            }
            Err(e) => ExecutionResult::failure(format!("Agent execution failed: {}", e)),
        }
    }

    /// Build the system prompt with automation context.
    fn build_system_prompt(&self, automation: &Automation, project: Option<&Project>) -> String {
        let mut prompt = String::new();

        prompt.push_str("You are an automation assistant for the Tasks platform. ");
        prompt.push_str("You execute automated workflows to help maintain and improve projects.\n\n");

        // Add project context if available
        if let Some(project) = project {
            prompt.push_str("## Project Context\n\n");
            prompt.push_str(&format!("- **Repository**: {}\n", project.repo));
            prompt.push_str(&format!("- **Default Branch**: {}\n", project.default_branch));
            prompt.push('\n');
        }

        // Add automation context
        prompt.push_str("## Automation Details\n\n");
        prompt.push_str(&format!("- **Name**: {}\n", automation.name));
        prompt.push_str(&format!("- **ID**: {}\n", automation.id));

        // Add trigger info
        let trigger_desc = match &automation.trigger {
            crate::model::automation::TriggerType::Schedule { cron } => {
                format!("Scheduled (cron: {})", cron)
            }
            crate::model::automation::TriggerType::Event { event_type } => {
                format!("Event-driven ({})", event_type)
            }
            crate::model::automation::TriggerType::Manual => "Manual trigger".to_string(),
        };
        prompt.push_str(&format!("- **Trigger**: {}\n\n", trigger_desc));

        // Add guidelines
        prompt.push_str("## Guidelines\n\n");
        prompt.push_str("1. Execute the automation prompt below to the best of your ability.\n");
        prompt.push_str("2. Be concise but thorough in your response.\n");
        prompt.push_str("3. If the automation requires actions you cannot perform, explain what would need to be done.\n");
        prompt.push_str("4. Report any issues or concerns clearly.\n\n");

        prompt
    }

    /// Build the user prompt from the automation.
    fn build_user_prompt(&self, automation: &Automation) -> String {
        let mut prompt = String::new();

        prompt.push_str("## Automation Task\n\n");
        prompt.push_str(&automation.prompt);

        // If there's a compiled workflow, include it
        if let Some(ref workflow) = automation.compiled_workflow {
            prompt.push_str("\n\n## Compiled Workflow\n\n");
            prompt.push_str(workflow);
        }

        prompt.push_str("\n\nPlease execute this automation and provide your response.");

        prompt
    }
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
            "Check if all tests pass",
            TriggerType::Manual,
        )
    }

    fn test_project() -> Project {
        Project::new("proj-1", "owner/repo")
    }

    #[test]
    fn test_build_system_prompt_with_project() {
        let executor = AutomationExecutor::new(AnthropicProvider::new("test-key"));
        let automation = test_automation();
        let project = test_project();

        let prompt = executor.build_system_prompt(&automation, Some(&project));

        assert!(prompt.contains("owner/repo"));
        assert!(prompt.contains("Test Automation"));
        assert!(prompt.contains("Manual trigger"));
    }

    #[test]
    fn test_build_system_prompt_without_project() {
        let executor = AutomationExecutor::new(AnthropicProvider::new("test-key"));
        let automation = test_automation();

        let prompt = executor.build_system_prompt(&automation, None);

        assert!(!prompt.contains("Repository"));
        assert!(prompt.contains("Test Automation"));
    }

    #[test]
    fn test_build_user_prompt() {
        let executor = AutomationExecutor::new(AnthropicProvider::new("test-key"));
        let automation = test_automation();

        let prompt = executor.build_user_prompt(&automation);

        assert!(prompt.contains("Check if all tests pass"));
        assert!(prompt.contains("Automation Task"));
    }

    #[test]
    fn test_build_user_prompt_with_compiled_workflow() {
        let executor = AutomationExecutor::new(AnthropicProvider::new("test-key"));
        let mut automation = test_automation();
        automation.compiled_workflow = Some("Step 1: Run tests\nStep 2: Report".to_string());

        let prompt = executor.build_user_prompt(&automation);

        assert!(prompt.contains("Compiled Workflow"));
        assert!(prompt.contains("Step 1: Run tests"));
    }

    #[test]
    fn test_execution_result_success() {
        let result = ExecutionResult::success("Done!".to_string());
        assert!(result.success);
        assert_eq!(result.output, Some("Done!".to_string()));
        assert!(result.error.is_none());
    }

    #[test]
    fn test_execution_result_failure() {
        let result = ExecutionResult::failure("Something went wrong".to_string());
        assert!(!result.success);
        assert!(result.output.is_none());
        assert_eq!(result.error, Some("Something went wrong".to_string()));
    }

    #[test]
    fn test_with_model() {
        let executor = AutomationExecutor::new(AnthropicProvider::new("test-key"))
            .with_model("claude-opus-4-0-20250514");
        assert_eq!(executor.model, "claude-opus-4-0-20250514");
    }
}
