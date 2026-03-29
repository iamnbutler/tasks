//! Claude-backed orchestrator implementation.
//!
//! Uses the tasks-agent crate to talk to Claude API and tasks-github
//! to fetch PR data from GitHub.

use serde::Deserialize;
use tracing::{info, warn};

use crate::error::OrchestratorError;
use crate::orchestrator::Orchestrator;
use crate::prompt::{build_evaluation_prompt, build_deep_review_prompt, parse_pr_url, system_prompt};
use crate::types::{
    default_triage, AnswerConfidence, AwaySummary, ConflictContext, ConflictTriage,
    EvaluationContext, OrchestratorAction, QualityEvaluation, QuestionAnswer, QuestionContext,
    SystemContext,
};
use events::EventType;
use models::parked_question::ParkedQuestion;
use models::task::{Task, TaskSource};
use tasks_agent::{AnthropicProvider, CompletionConfig, CompletionRequest, Message, Provider};
use tasks_github::GitHubClient;

/// Default model for orchestrator evaluation.
const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// Default maximum tokens for evaluation response.
const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Orchestrator implementation backed by Claude.
///
/// Holds a Claude API provider for LLM calls and a GitHub client for
/// fetching PR and issue data.
pub struct ClaudeOrchestrator {
    provider: AnthropicProvider,
    github: GitHubClient,
    model: String,
    max_tokens: u32,
}

impl ClaudeOrchestrator {
    /// Create a new ClaudeOrchestrator with the given provider and GitHub client.
    pub fn new(provider: AnthropicProvider, github: GitHubClient) -> Self {
        Self {
            provider,
            github,
            model: DEFAULT_MODEL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Create from environment variables (ANTHROPIC_API_KEY, GITHUB_TOKEN).
    ///
    /// Optional env vars for LLM configuration:
    /// - `TASKS_ORCHESTRATOR_MODEL` — model name (default: `claude-opus-4-6`)
    /// - `TASKS_ORCHESTRATOR_MAX_TOKENS` — max response tokens (default: 32000)
    pub fn from_env() -> Result<Self, OrchestratorError> {
        let provider =
            AnthropicProvider::from_env().map_err(OrchestratorError::Agent)?;

        let github_token = std::env::var("GITHUB_TOKEN").map_err(|_| {
            OrchestratorError::GitHub("GITHUB_TOKEN environment variable not set".into())
        })?;
        let github = GitHubClient::new(github_token);

        let mut instance = Self::new(provider, github);

        if let Ok(model) = std::env::var("TASKS_ORCHESTRATOR_MODEL") {
            instance.model = model;
        }
        if let Ok(max_tokens) = std::env::var("TASKS_ORCHESTRATOR_MAX_TOKENS") {
            if let Ok(val) = max_tokens.parse::<u32>() {
                instance.max_tokens = val;
            }
        }

        Ok(instance)
    }

    /// Set a custom model for evaluation.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set custom max tokens for evaluation responses.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
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

        if parsed.reasoning.trim().is_empty() {
            return Err(OrchestratorError::Evaluation(
                "Evaluation reasoning must not be empty".to_string(),
            ));
        }

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
#[derive(Debug)]
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

        let config = CompletionConfig::new(&self.model).with_max_tokens(self.max_tokens);
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
        if !pass1.needs_deeper_review
            || pass1.files_to_review.is_none()
            || pass1.files_to_review.as_ref().is_some_and(|f| f.is_empty())
        {
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

        let config = CompletionConfig::new(&self.model).with_max_tokens(self.max_tokens);
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

    async fn think(
        &self,
        context: &SystemContext,
    ) -> Result<Vec<OrchestratorAction>, OrchestratorError> {
        // Rule-based reasoning pass — no LLM calls.
        // Survey system state and recent events, identify patterns, narrate.
        let mut actions = Vec::new();

        // Narrate recent events the human should know about
        for event in &context.recent_events {
            match &event.event_type {
                EventType::TaskStateFailed => {
                    let reason = event
                        .data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown reason");
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Task {} failed: {}",
                        event.task, reason
                    )));
                }
                EventType::TaskStateCompleted => {
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Task {} completed.",
                        event.task
                    )));
                }
                EventType::MergeCompleted => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Merged PR for task {}.",
                        task_id
                    )));
                }
                EventType::MergeConflict => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Conflict detected on task {}.",
                        task_id
                    )));
                }
                EventType::SystemModePlay => {
                    actions.push(OrchestratorAction::EmitThought(
                        "Mode changed to Play — fully autonomous.".to_string(),
                    ));
                }
                EventType::SystemModePause => {
                    actions.push(OrchestratorAction::EmitThought(
                        "Mode changed to Pause — holding approved merges.".to_string(),
                    ));
                }
                EventType::SystemModeStop => {
                    actions.push(OrchestratorAction::EmitThought(
                        "Mode changed to Stop — idle.".to_string(),
                    ));
                }
                _ => {}
            }
        }

        // Pattern detection: multiple failures in the same area
        let recent_failures: Vec<&events::Event> = context
            .recent_events
            .iter()
            .filter(|e| e.event_type == EventType::TaskStateFailed)
            .collect();
        if recent_failures.len() >= 3 {
            actions.push(OrchestratorAction::EmitThought(format!(
                "{} tasks failed recently — possible systemic issue.",
                recent_failures.len()
            )));
        }

        // Surface long-running sessions
        use models::task::TaskState;
        let long_running: Vec<&Task> = context
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .filter(|t| {
                t.last_activity_at
                    .map(|at| (chrono::Utc::now() - at).num_minutes() > 30)
                    .unwrap_or(false)
            })
            .collect();
        for task in &long_running {
            let mins = task
                .last_activity_at
                .map(|at| (chrono::Utc::now() - at).num_minutes())
                .unwrap_or(0);
            actions.push(OrchestratorAction::EmitThought(format!(
                "Task {} has been running with no activity for {}m.",
                &task.id[..8.min(task.id.len())],
                mins
            )));
        }

        Ok(actions)
    }

    async fn answer_question(
        &self,
        context: &QuestionContext,
    ) -> Result<String, OrchestratorError> {
        let result = self.answer_question_with_confidence(context).await?;
        Ok(result.answer)
    }

    async fn answer_question_with_confidence(
        &self,
        context: &QuestionContext,
    ) -> Result<QuestionAnswer, OrchestratorError> {
        info!(
            task_id = %context.task.id,
            question_len = context.question.len(),
            human_present = context.human_present,
            "Answering stuck agent's question with confidence assessment"
        );

        let system = build_question_answer_system_prompt_with_confidence();

        let user_prompt = build_question_answer_prompt(
            &context.task.title,
            context.task.description.as_deref(),
            &context.question,
            &context.project.repo,
        );

        let config = CompletionConfig::new(&self.model).with_max_tokens(4096);
        let request = CompletionRequest::new(config, vec![Message::user(user_prompt)])
            .with_system(system);

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(OrchestratorError::Agent)?;

        let text = response.text().trim().to_string();

        // Try to parse structured response with confidence
        let result = parse_confidence_answer(&text);

        info!(
            task_id = %context.task.id,
            answer_len = result.answer.len(),
            confidence = ?result.confidence,
            "Generated answer for stuck agent"
        );

        Ok(result)
    }

    async fn generate_away_summary(
        &self,
        events_while_away: &[events::Event],
        parked_questions: Vec<ParkedQuestion>,
        away_seconds: i64,
    ) -> Result<AwaySummary, OrchestratorError> {
        let mut prs_merged = 0u32;
        let mut prs_rejected = 0u32;
        let mut questions_answered = 0u32;
        let mut conflicts_resolved = 0u32;

        for event in events_while_away {
            match event.event_type {
                EventType::MergeCompleted => prs_merged += 1,
                EventType::MergeRejected | EventType::MergeChangesRequested => {
                    prs_rejected += 1;
                }
                EventType::OrchestratorFeedback => {
                    let action = event.data.get("action").and_then(|v| v.as_str());
                    match action {
                        Some("question_answer") => questions_answered += 1,
                        Some("mechanical_rebase") | Some("auto_resolve") => {
                            conflicts_resolved += 1;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let questions_parked = parked_questions.len() as u32;

        // Build human-readable duration
        let duration = if away_seconds < 60 {
            format!("{}s", away_seconds)
        } else if away_seconds < 3600 {
            format!("{}m", away_seconds / 60)
        } else {
            format!(
                "{}h {}m",
                away_seconds / 3600,
                (away_seconds % 3600) / 60
            )
        };

        let mut parts = Vec::new();
        if prs_merged > 0 {
            parts.push(format!(
                "merged {} PR{}",
                prs_merged,
                if prs_merged == 1 { "" } else { "s" }
            ));
        }
        if prs_rejected > 0 {
            parts.push(format!(
                "rejected {} PR{}",
                prs_rejected,
                if prs_rejected == 1 { "" } else { "s" }
            ));
        }
        if questions_answered > 0 {
            parts.push(format!(
                "answered {} agent question{}",
                questions_answered,
                if questions_answered == 1 { "" } else { "s" }
            ));
        }
        if conflicts_resolved > 0 {
            parts.push(format!(
                "resolved {} conflict{}",
                conflicts_resolved,
                if conflicts_resolved == 1 { "" } else { "s" }
            ));
        }
        if questions_parked > 0 {
            parts.push(format!(
                "{} question{} need{} your input",
                questions_parked,
                if questions_parked == 1 { "" } else { "s" },
                if questions_parked == 1 { "s" } else { "" }
            ));
        }

        let message = if parts.is_empty() {
            format!("While you were gone ({duration}): nothing notable happened.")
        } else {
            format!("While you were gone ({duration}): {}.", parts.join(", "))
        };

        Ok(AwaySummary {
            away_duration_seconds: away_seconds,
            prs_merged,
            prs_rejected,
            questions_answered,
            questions_parked,
            conflicts_resolved,
            parked_questions,
            message,
        })
    }
}

/// Build the system prompt for answering agent questions with confidence assessment.
fn build_question_answer_system_prompt_with_confidence() -> String {
    r#"You are a project foreman helping an implementor who is stuck on a coding task.
The human operator is currently away, so you are making autonomous decisions.

Your role:
- Give concise, actionable guidance — the agent needs to move forward, not read an essay.
- Be specific: point to files, functions, patterns, or approaches.
- If the question reveals a misunderstanding, correct it directly.
- If the question is about a design decision, make the call — don't equivocate.
- Keep your answer under 500 words. Shorter is better.

IMPORTANT: You must also assess your confidence in the answer.
Start your response with a confidence tag on its own line:
- [CONFIDENCE: high] — You're confident this is the right guidance (technical questions with clear answers, standard patterns)
- [CONFIDENCE: medium] — You can give reasonable guidance but it involves judgment calls
- [CONFIDENCE: low] — The question requires human judgment (business decisions, unclear requirements, security-sensitive choices, questions about user preferences)

Then provide your answer on the following lines.

Do NOT:
- Repeat the question back
- Give vague advice like "consider the tradeoffs"
- Suggest the agent ask someone else
- Write code unless it directly answers the question"#.to_string()
}

/// Parse a confidence-tagged answer from the LLM response.
fn parse_confidence_answer(text: &str) -> QuestionAnswer {
    let text = text.trim();

    // Try to extract [CONFIDENCE: level] tag
    if let Some(rest) = text.strip_prefix("[CONFIDENCE:") {
        if let Some(end) = rest.find(']') {
            let level = rest[..end].trim().to_lowercase();
            let answer = rest[end + 1..].trim().to_string();
            let confidence = match level.as_str() {
                "high" => AnswerConfidence::High,
                "medium" => AnswerConfidence::Medium,
                "low" => AnswerConfidence::Low,
                _ => AnswerConfidence::Medium,
            };
            let park_reason = if confidence == AnswerConfidence::Low {
                Some(
                    "The orchestrator assessed low confidence in answering this question autonomously."
                        .to_string(),
                )
            } else {
                None
            };
            return QuestionAnswer {
                answer,
                confidence,
                park_reason,
            };
        }
    }

    // No confidence tag found — default to medium
    QuestionAnswer {
        answer: text.to_string(),
        confidence: AnswerConfidence::Medium,
        park_reason: None,
    }
}

/// Build the user prompt for answering an agent's question.
fn build_question_answer_prompt(
    task_title: &str,
    task_description: Option<&str>,
    question: &str,
    repo: &str,
) -> String {
    let mut prompt = format!(
        "## Task\n\
         **Repository:** {repo}\n\
         **Title:** {task_title}\n"
    );

    if let Some(desc) = task_description {
        prompt.push_str(&format!("**Description:** {desc}\n"));
    }

    prompt.push_str(&format!(
        "\n## Agent's Question\n\
         {question}\n\
         \n## Instructions\n\
         Answer the agent's question with specific, actionable guidance so they can continue working."
    ));

    prompt
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

    /// Create a ClaudeOrchestrator for unit tests (no real API calls needed).
    fn make_test_orchestrator() -> ClaudeOrchestrator {
        ClaudeOrchestrator {
            provider: AnthropicProvider::new("test-key"),
            github: GitHubClient::new("test-token"),
            model: DEFAULT_MODEL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

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
    fn test_parse_evaluation_rejects_empty_reasoning() {
        let orchestrator = make_test_orchestrator();
        let text = r#"{"approved": true, "reasoning": "", "feedback": null}"#;
        let result = orchestrator.parse_evaluation_response(text);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("reasoning must not be empty"), "got: {err}");
    }

    #[test]
    fn test_parse_evaluation_rejects_whitespace_reasoning() {
        let orchestrator = make_test_orchestrator();
        let text = r#"{"approved": true, "reasoning": "   ", "feedback": null}"#;
        let result = orchestrator.parse_evaluation_response(text);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_evaluation_accepts_valid_reasoning() {
        let orchestrator = make_test_orchestrator();
        let text = r#"{"approved": true, "reasoning": "Changes look correct", "feedback": null}"#;
        let result = orchestrator.parse_evaluation_response(text);
        assert!(result.is_ok());
        let eval = result.unwrap();
        assert!(eval.approved);
        assert_eq!(eval.reasoning, "Changes look correct");
    }

    #[test]
    fn test_extract_json_none() {
        assert!(extract_json("no json here").is_none());
        assert!(extract_json("incomplete { json").is_none());
    }

    #[test]
    fn test_parse_confidence_high() {
        let text = "[CONFIDENCE: high]\nUse the standard pattern from the codebase.";
        let result = parse_confidence_answer(text);
        assert_eq!(result.confidence, AnswerConfidence::High);
        assert_eq!(result.answer, "Use the standard pattern from the codebase.");
        assert!(result.park_reason.is_none());
    }

    #[test]
    fn test_parse_confidence_low() {
        let text = "[CONFIDENCE: low]\nThis depends on business requirements.";
        let result = parse_confidence_answer(text);
        assert_eq!(result.confidence, AnswerConfidence::Low);
        assert!(result.park_reason.is_some());
    }

    #[test]
    fn test_parse_confidence_missing_tag() {
        let text = "Just do it this way.";
        let result = parse_confidence_answer(text);
        assert_eq!(result.confidence, AnswerConfidence::Medium);
        assert_eq!(result.answer, "Just do it this way.");
    }

    #[test]
    fn test_parse_confidence_medium() {
        let text = "[CONFIDENCE: medium]\nI'd suggest approach A but B could also work.";
        let result = parse_confidence_answer(text);
        assert_eq!(result.confidence, AnswerConfidence::Medium);
        assert!(result.park_reason.is_none());
    }
}
