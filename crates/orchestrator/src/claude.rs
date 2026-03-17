//! Claude-backed orchestrator implementation.
//!
//! Uses the tasks-agent crate to talk to Claude API and tasks-github
//! to fetch PR data from GitHub.

use tracing::info;

use crate::error::OrchestratorError;
use crate::orchestrator::Orchestrator;
use crate::types::{EvaluationContext, QualityEvaluation};
use models::task::Task;
use tasks_agent::AnthropicProvider;

/// Orchestrator implementation backed by Claude.
///
/// Holds a Claude API provider for LLM calls. In future, will also
/// hold a GitHub client for fetching PR data.
pub struct ClaudeOrchestrator {
    #[allow(dead_code)]
    provider: AnthropicProvider,
}

impl ClaudeOrchestrator {
    /// Create a new ClaudeOrchestrator with the given provider.
    pub fn new(provider: AnthropicProvider) -> Self {
        Self { provider }
    }

    /// Create from environment variables (ANTHROPIC_API_KEY).
    pub fn from_env() -> Result<Self, OrchestratorError> {
        let provider = AnthropicProvider::from_env()
            .map_err(|e| OrchestratorError::Agent(e))?;
        Ok(Self::new(provider))
    }
}

impl Orchestrator for ClaudeOrchestrator {
    async fn evaluate(
        &self,
        context: &EvaluationContext,
    ) -> Result<QualityEvaluation, OrchestratorError> {
        info!(
            task_id = %context.task.id,
            pr_url = %context.entry.pr_url,
            project = %context.project.repo,
            "Evaluating merge queue entry"
        );

        // TODO(#81): Implement actual quality evaluation with LLM.
        // For now, return a placeholder that signals "not yet implemented".
        Ok(QualityEvaluation {
            approved: false,
            reasoning: "Orchestrator evaluation not yet implemented".to_string(),
            feedback: None,
        })
    }

    async fn feedback(
        &self,
        task: &Task,
        feedback: &str,
    ) -> Result<(), OrchestratorError> {
        info!(
            task_id = %task.id,
            feedback_len = feedback.len(),
            "Sending feedback to task"
        );

        // TODO(#81): Implement actual feedback delivery via agent session.
        Ok(())
    }
}
