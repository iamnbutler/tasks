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
    default_triage, ConflictContext, ConflictTriage, EvaluationContext, OrchestratorAction,
    QualityEvaluation, QuestionContext, SystemContext,
};
use events::EventType;
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
        use crate::prompt::truncate_text;
        use models::task::TaskState;

        // Rule-based reasoning pass — no LLM calls.
        // Survey system state and recent events, identify patterns, narrate.
        // This is the orchestrator's stream of consciousness: it narrates what
        // it sees so the human can glance at the feed and understand the pulse.
        let mut actions = Vec::new();

        // --- Narrate recent events the human should know about ---
        for event in &context.recent_events {
            match &event.event_type {
                EventType::TaskStateFailed => {
                    let reason = event
                        .data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown reason");
                    let task_title = event
                        .data
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    if task_title.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Task {short_id} failed: {reason}"
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Task {short_id} ({}) failed: {reason}",
                            truncate_text(task_title, 60)
                        )));
                    }
                }
                EventType::TaskStateCompleted => {
                    let task_title = event
                        .data
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    if task_title.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Task {short_id} completed."
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Task {short_id} ({}) completed.",
                            truncate_text(task_title, 60)
                        )));
                    }
                }
                EventType::TaskStateRunning => {
                    let task_title = event
                        .data
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    if !task_title.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Task {short_id} started: {}",
                            truncate_text(task_title, 80)
                        )));
                    }
                }
                EventType::TaskStateQuestion => {
                    let question = event
                        .data
                        .get("question")
                        .or_else(|| event.data.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    if question.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Agent on task {short_id} is stuck and asking for help."
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Agent on task {short_id} is stuck: {}",
                            truncate_text(question, 120)
                        )));
                    }
                }
                EventType::TaskStateConflict => {
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Task {short_id} hit a merge conflict — investigating resolution strategy."
                    )));
                }
                EventType::TaskStateAwaitingMerge => {
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Task {short_id} is ready — PR queued for merge evaluation."
                    )));
                }
                EventType::TaskStateChangesRequested => {
                    let short_id = &event.task[..event.task.floor_char_boundary(8)];
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Sent changes back to task {short_id} — agent should address feedback."
                    )));
                }
                EventType::MergeQueued => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    let short_id = &task_id[..task_id.floor_char_boundary(8)];
                    let pr_url = event
                        .data
                        .get("pr_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Extract PR number from URL if possible
                    let pr_label = pr_url
                        .rsplit('/')
                        .next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .map(|n| format!("PR #{n}"))
                        .unwrap_or_default();
                    if pr_label.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "New PR from task {short_id} entered the merge queue."
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "{pr_label} from task {short_id} entered the merge queue — will evaluate shortly."
                        )));
                    }
                }
                EventType::MergeCompleted => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    let short_id = &task_id[..task_id.floor_char_boundary(8)];
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Merged PR for task {short_id} into main."
                    )));
                }
                EventType::MergeConflict => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    let short_id = &task_id[..task_id.floor_char_boundary(8)];
                    actions.push(OrchestratorAction::EmitThought(format!(
                        "Merge conflict detected on task {short_id} — triaging resolution."
                    )));
                }
                EventType::MergeRejected => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    let short_id = &task_id[..task_id.floor_char_boundary(8)];
                    let reason = event
                        .data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if reason.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Rejected PR for task {short_id}."
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Rejected PR for task {short_id}: {}",
                            truncate_text(reason, 150)
                        )));
                    }
                }
                EventType::OrchestratorDecision => {
                    // Narrate our own evaluation decisions with reasoning
                    let approved = event
                        .data
                        .get("approved")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let reasoning = event
                        .data
                        .get("reasoning")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let entry_id = event
                        .data
                        .get("entry_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let short_entry = &entry_id[..entry_id.floor_char_boundary(8)];
                    let verdict = if approved { "Approved" } else { "Rejected" };
                    if reasoning.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "{verdict} merge entry {short_entry}."
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "{verdict} merge entry {short_entry} — {}",
                            truncate_text(reasoning, 150)
                        )));
                    }
                }
                EventType::OrchestratorFeedback => {
                    let task_id = event
                        .data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&event.task);
                    let short_id = &task_id[..task_id.floor_char_boundary(8)];
                    let context_str = event
                        .data
                        .get("context")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match context_str {
                        "question_answer" => {
                            actions.push(OrchestratorAction::EmitThought(format!(
                                "Answered stuck agent on task {short_id} — unblocking."
                            )));
                        }
                        _ => {
                            actions.push(OrchestratorAction::EmitThought(format!(
                                "Sent feedback to agent on task {short_id}."
                            )));
                        }
                    }
                }
                EventType::SystemModePlay => {
                    actions.push(OrchestratorAction::EmitThought(
                        "Mode changed to Play — fully autonomous. I'll evaluate and merge PRs as they come in.".to_string(),
                    ));
                }
                EventType::SystemModePause => {
                    actions.push(OrchestratorAction::EmitThought(
                        "Mode changed to Pause — I'll evaluate PRs but hold merges for your approval.".to_string(),
                    ));
                }
                EventType::SystemModeStop => {
                    actions.push(OrchestratorAction::EmitThought(
                        "Mode changed to Stop — going idle. No evaluations or merges until resumed.".to_string(),
                    ));
                }
                EventType::OrchestratorEscalation => {
                    let action = event
                        .data
                        .get("action")
                        .and_then(|v| v.as_str())
                        .unwrap_or("escalation");
                    let message = event
                        .data
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if message.is_empty() {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Escalating: {action}"
                        )));
                    } else {
                        actions.push(OrchestratorAction::EmitThought(format!(
                            "Escalating — {}",
                            truncate_text(message, 150)
                        )));
                    }
                }
                _ => {}
            }
        }

        // --- Pattern detection: multiple failures ---
        let recent_failures: Vec<&events::Event> = context
            .recent_events
            .iter()
            .filter(|e| e.event_type == EventType::TaskStateFailed)
            .collect();
        if recent_failures.len() >= 3 {
            // Check if failures share error patterns
            let reasons: Vec<&str> = recent_failures
                .iter()
                .filter_map(|e| e.data.get("reason").and_then(|v| v.as_str()))
                .collect();
            let has_common_pattern = reasons.len() >= 2 && {
                // Simple heuristic: check if any two reasons share a common substring
                reasons.windows(2).any(|w| {
                    let a = w[0].to_lowercase();
                    let b = w[1].to_lowercase();
                    a.contains("timeout") && b.contains("timeout")
                        || a.contains("connection") && b.contains("connection")
                        || a.contains("permission") && b.contains("permission")
                        || a.contains("memory") && b.contains("memory")
                })
            };
            if has_common_pattern {
                actions.push(OrchestratorAction::EmitThought(format!(
                    "{} tasks failed with a similar pattern — likely a systemic issue, not individual task problems.",
                    recent_failures.len()
                )));
            } else {
                actions.push(OrchestratorAction::EmitThought(format!(
                    "{} tasks failed recently — watching for patterns.",
                    recent_failures.len()
                )));
            }
        }

        // --- Surface long-running sessions ---
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
            let short_id = &task.id[..task.id.floor_char_boundary(8)];
            actions.push(OrchestratorAction::EmitThought(format!(
                "Task {short_id} has been running with no activity for {mins}m — may be stuck."
            )));
        }

        // --- Periodic project health summary ---
        // Emit a brief status summary when there's been a meaningful
        // amount of activity (first think after startup, or enough events).
        let is_first_think = context.last_think_at.is_none();
        let had_significant_activity = context.recent_events.len() >= 5;

        if is_first_think || had_significant_activity {
            let total = context.tasks.len();
            if total > 0 {
                let running = context.tasks.iter().filter(|t| t.state == TaskState::Running).count();
                let completed = context.tasks.iter().filter(|t| t.state == TaskState::Completed).count();
                let failed = context.tasks.iter().filter(|t| t.state == TaskState::Failed).count();
                let waiting = context.tasks.iter().filter(|t| t.state == TaskState::Waiting).count();
                let stuck = context.tasks.iter().filter(|t| {
                    matches!(t.state, TaskState::Question | TaskState::Conflict | TaskState::Blocked)
                }).count();
                let queue_len = context.merge_queue.len();

                let mut parts = Vec::new();
                if running > 0 { parts.push(format!("{running} running")); }
                if waiting > 0 { parts.push(format!("{waiting} waiting")); }
                if stuck > 0 { parts.push(format!("{stuck} stuck")); }
                if completed > 0 { parts.push(format!("{completed} completed")); }
                if failed > 0 { parts.push(format!("{failed} failed")); }

                let mut summary = format!("Project pulse: {}", parts.join(", "));
                if queue_len > 0 {
                    summary.push_str(&format!(". Merge queue: {queue_len} pending."));
                } else {
                    summary.push_str(". Merge queue clear.");
                }
                actions.push(OrchestratorAction::EmitThought(summary));
            }
        }

        Ok(actions)
    }

    async fn answer_question(
        &self,
        context: &QuestionContext,
    ) -> Result<String, OrchestratorError> {
        info!(
            task_id = %context.task.id,
            question_len = context.question.len(),
            "Answering stuck agent's question"
        );

        let system = build_question_answer_system_prompt();

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

        let answer = response.text().trim().to_string();

        info!(
            task_id = %context.task.id,
            answer_len = answer.len(),
            "Generated answer for stuck agent"
        );

        Ok(answer)
    }
}

/// Build the system prompt for answering agent questions.
fn build_question_answer_system_prompt() -> String {
    r#"You are a project foreman helping an implementor who is stuck on a coding task.

Your role:
- Give concise, actionable guidance — the agent needs to move forward, not read an essay.
- Be specific: point to files, functions, patterns, or approaches.
- If the question reveals a misunderstanding, correct it directly.
- If the question is about a design decision, make the call — don't equivocate.
- Keep your answer under 500 words. Shorter is better.

Do NOT:
- Repeat the question back
- Give vague advice like "consider the tradeoffs"
- Suggest the agent ask someone else
- Write code unless it directly answers the question"#.to_string()
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

/// Truncate text for logging (UTF-8 safe).
fn truncate(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        text
    } else {
        let boundary = text.floor_char_boundary(max_len);
        &text[..boundary]
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
}
