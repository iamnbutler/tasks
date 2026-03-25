//! Claude-backed orchestrator implementation.
//!
//! Uses the tasks-agent crate to talk to Claude API and tasks-github
//! to fetch PR data from GitHub.

use serde::Deserialize;
use tracing::{info, warn};

use crate::error::OrchestratorError;
use crate::orchestrator::Orchestrator;
use crate::prompt::{build_evaluation_prompt, build_deep_review_prompt, parse_pr_url, system_prompt};
use crate::types::{default_triage, ConflictContext, ConflictTriage, EvaluationContext, QualityEvaluation};
use models::task::{Task, TaskSource};
use tasks_agent::{AnthropicProvider, CompletionConfig, CompletionRequest, Message, Provider};
use tasks_github::GitHubClient;

/// Default model for orchestrator evaluation.
const DEFAULT_MODEL: &str = "claude-opus-4-5";

/// Maximum tokens for evaluation response.
const MAX_TOKENS: u32 = 50_000;

/// Orchestrator implementation backed by Claude.
///
/// Holds a Claude API provider for LLM calls and a GitHub client for
/// fetching PR and issue data.
pub struct ClaudeOrchestrator {
    provider: AnthropicProvider,
    github: GitHubClient,
    model: String,
}

impl ClaudeOrchestrator {
    /// Create a new ClaudeOrchestrator with the given provider and GitHub client.
    pub fn new(provider: AnthropicProvider, github: GitHubClient) -> Self {
        Self {
            provider,
            github,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Create from environment variables (ANTHROPIC_API_KEY, GITHUB_TOKEN).
    pub fn from_env() -> Result<Self, OrchestratorError> {
        let provider =
            AnthropicProvider::from_env().map_err(OrchestratorError::Agent)?;

        let github_token = std::env::var("GITHUB_TOKEN").map_err(|_| {
            OrchestratorError::GitHub("GITHUB_TOKEN environment variable not set".into())
        })?;
        let github = GitHubClient::new(github_token);

        Ok(Self::new(provider, github))
    }

    /// Set a custom model for evaluation.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Parse the LLM response into a ParsedEvaluation (includes triage fields).
    fn parse_evaluation_response(&self, text: &str) -> Result<ParsedEvaluation, OrchestratorError> {
        let json_str = extract_json(text).ok_or_else(|| {
            OrchestratorError::Evaluation(format!(
                "Could not find JSON in response: {}",
                truncate(text, 200)
            ))
        })?;

        let parsed: EvaluationResponse = serde_json::from_str(json_str).map_err(|e| {
            OrchestratorError::Evaluation(format!("Failed to parse evaluation JSON: {}", e))
        })?;

        Ok(ParsedEvaluation {
            approved: parsed.approved,
            needs_deeper_review: parsed.needs_deeper_review,
            reasoning: parsed.reasoning,
            feedback: parsed.feedback,
            files_to_review: parsed.files_to_review,
        })
    }
}

/// Internal struct for parsing the LLM's JSON response.
#[derive(Deserialize)]
struct EvaluationResponse {
    approved: bool,
    #[serde(default)]
    needs_deeper_review: bool,
    reasoning: String,
    feedback: Option<String>,
    #[serde(default)]
    files_to_review: Option<Vec<String>>,
}

/// Internal parsed result from the LLM (includes triage fields not in QualityEvaluation).
struct ParsedEvaluation {
    approved: bool,
    needs_deeper_review: bool,
    reasoning: String,
    feedback: Option<String>,
    files_to_review: Option<Vec<String>>,
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

        // Parse PR URL to get owner/repo/number
        let (owner, repo, pr_number) = parse_pr_url(&context.entry.pr_url).ok_or_else(|| {
            OrchestratorError::Evaluation(format!(
                "Invalid PR URL format: {}",
                context.entry.pr_url
            ))
        })?;

        // Fetch the PR metadata from GitHub
        let pr = self
            .github
            .get_pull_request(&owner, &repo, pr_number)
            .await
            .map_err(|e| OrchestratorError::GitHub(e.to_string()))?;

        info!(
            pr_number = pr.number,
            pr_title = %pr.title,
            mergeable = ?pr.mergeable,
            review_decision = ?pr.review_decision,
            "Fetched PR details"
        );

        // Fetch the actual diff
        let diff = match self.github.get_pr_diff(&owner, &repo, pr_number).await {
            Ok(d) => {
                info!(diff_len = d.len(), "Fetched PR diff");
                Some(d)
            }
            Err(e) => {
                warn!(error = %e, "Failed to fetch PR diff, continuing without it");
                None
            }
        };

        // If the task originated from a GitHub issue, fetch it
        let issue = match &context.task.source {
            TaskSource::GithubIssue {
                owner: issue_owner,
                repo: issue_repo,
                number,
            } => {
                match self
                    .github
                    .get_issue(issue_owner, issue_repo, *number)
                    .await
                {
                    Ok(issue) => {
                        info!(issue_number = issue.number, "Fetched associated issue");
                        Some(issue)
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to fetch associated issue, continuing without it");
                        None
                    }
                }
            }
            _ => None,
        };

        // --- Pass 1: Triage with diff ---
        let system = system_prompt();
        let user_prompt = build_evaluation_prompt(
            &pr,
            issue.as_ref(),
            &context.task.title,
            context.task.description.as_deref(),
            diff.as_deref(),
            &context.queue_context,
        );

        let config = CompletionConfig::new(&self.model).with_max_tokens(MAX_TOKENS);
        let request = CompletionRequest::new(config, vec![Message::user(user_prompt)])
            .with_system(system.clone());

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(OrchestratorError::Agent)?;

        let response_text = response.text();
        info!(response_len = response_text.len(), "Pass 1 evaluation response");

        let pass1 = self.parse_evaluation_response(&response_text)?;

        // If pass 1 is decisive (no deeper review needed), return immediately
        if !pass1.needs_deeper_review || pass1.files_to_review.is_none() {
            info!(
                approved = pass1.approved,
                has_feedback = pass1.feedback.is_some(),
                "Evaluation complete (pass 1 — no deeper review needed)"
            );
            return Ok(QualityEvaluation {
                approved: pass1.approved,
                reasoning: pass1.reasoning,
                feedback: pass1.feedback,
            });
        }

        // --- Pass 2: Deep review ---
        let files_requested = pass1.files_to_review.unwrap_or_default();
        info!(
            files = ?files_requested,
            "Pass 1 requested deeper review, fetching files"
        );

        // Fetch requested files from the PR branch
        let mut files: Vec<(String, String)> = Vec::new();
        for file_path in files_requested.iter().take(5) {
            match self
                .github
                .get_file_content_at_ref(&owner, &repo, file_path, &pr.head_ref)
                .await
            {
                Ok(Some(content)) => {
                    info!(path = %file_path, len = content.len(), "Fetched file for deep review");
                    files.push((file_path.clone(), content));
                }
                Ok(None) => {
                    info!(path = %file_path, "File not found on PR branch");
                    files.push((file_path.clone(), "(file not found)".to_string()));
                }
                Err(e) => {
                    warn!(path = %file_path, error = %e, "Failed to fetch file for deep review");
                    files.push((file_path.clone(), format!("(fetch error: {e})")));
                }
            }
        }

        let deep_prompt = build_deep_review_prompt(
            &pr,
            issue.as_ref(),
            &context.task.title,
            context.task.description.as_deref(),
            diff.as_deref().unwrap_or("(no diff available)"),
            &pass1.reasoning,
            &files,
            &context.queue_context,
        );

        let config = CompletionConfig::new(&self.model).with_max_tokens(MAX_TOKENS);
        let request = CompletionRequest::new(config, vec![Message::user(deep_prompt)])
            .with_system(system);

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(OrchestratorError::Agent)?;

        let response_text = response.text();
        info!(response_len = response_text.len(), "Pass 2 evaluation response");

        let pass2 = self.parse_evaluation_response(&response_text)?;

        info!(
            approved = pass2.approved,
            has_feedback = pass2.feedback.is_some(),
            "Evaluation complete (pass 2 — deep review)"
        );

        Ok(QualityEvaluation {
            approved: pass2.approved,
            reasoning: pass2.reasoning,
            feedback: pass2.feedback,
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

        // TODO: Implement actual feedback delivery via agent session.
        // This will require integration with the session management system.
        // For now, log the feedback that would be sent.
        info!(
            task_id = %task.id,
            feedback = %feedback,
            "Feedback would be sent to agent session (not yet implemented)"
        );

        Ok(())
    }

    async fn triage_conflict(
        &self,
        context: &ConflictContext,
    ) -> Result<ConflictTriage, OrchestratorError> {
        info!(
            task_id = %context.task.id,
            conflict_type = ?context.conflict_info.conflict_type,
            mode = ?context.mode,
            human_present = context.human_present,
            "Triaging merge conflict"
        );

        // Use the default triage logic. This could be enhanced in the future
        // to use an LLM for more sophisticated conflict analysis.
        let triage = default_triage(&context.conflict_info, context.mode, context.human_present);

        info!(
            task_id = %context.task.id,
            resolution = ?triage.resolution,
            reasoning = %triage.reasoning,
            "Conflict triage complete"
        );

        Ok(triage)
    }
}

/// Extract JSON from a text response.
///
/// Looks for content between `{` and `}` braces, handling potential
/// markdown code blocks.
fn extract_json(text: &str) -> Option<&str> {
    // Try to find JSON in a code block first
    if let Some(start) = text.find("```json") {
        let json_start = start + 7;
        if let Some(end) = text[json_start..].find("```") {
            return Some(text[json_start..json_start + end].trim());
        }
    }

    // Try to find JSON in a generic code block
    if let Some(start) = text.find("```") {
        let block_start = start + 3;
        // Skip language identifier if present
        let content_start = text[block_start..]
            .find('\n')
            .map(|i| block_start + i + 1)
            .unwrap_or(block_start);
        if let Some(end) = text[content_start..].find("```") {
            let candidate = text[content_start..content_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try to find raw JSON object
    if let Some(start) = text.find('{') {
        // Find the matching closing brace
        let mut depth = 0;
        let mut in_string = false;
        let mut escape_next = false;

        for (i, c) in text[start..].char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }

            match c {
                '\\' if in_string => escape_next = true,
                '"' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[start..start + i + 1]);
                    }
                }
                _ => {}
            }
        }
    }

    None
}

/// Truncate text for logging.
fn truncate(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        text
    } else {
        &text[..max_len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_raw() {
        let text = r#"Here is my evaluation:
{"approved": true, "reasoning": "Looks good", "feedback": null}
That's all."#;
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
        assert!(json.contains("approved"));
    }

    #[test]
    fn test_extract_json_code_block() {
        let text = r#"Here is my evaluation:

```json
{"approved": false, "reasoning": "Tests failing", "feedback": "Fix the tests"}
```

That's all."#;
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
        assert!(json.contains("approved"));
    }

    #[test]
    fn test_extract_json_generic_code_block() {
        let text = r#"```
{"approved": true, "reasoning": "OK", "feedback": null}
```"#;
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
    }

    #[test]
    fn test_extract_json_nested() {
        let text = r#"{"outer": {"inner": "value"}, "approved": true}"#;
        let json = extract_json(text).unwrap();
        assert_eq!(json, text);
    }

    #[test]
    fn test_extract_json_with_strings() {
        let text = r#"{"reasoning": "The PR has { braces } in description", "approved": true}"#;
        let json = extract_json(text).unwrap();
        assert_eq!(json, text);
    }

    #[test]
    fn test_extract_json_none() {
        assert!(extract_json("no json here").is_none());
        assert!(extract_json("incomplete { json").is_none());
    }
}
