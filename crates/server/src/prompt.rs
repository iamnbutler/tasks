//! Prompt construction for agent sessions (spec §15).
//!
//! Builds a Markdown prompt from task details, project context, and behavioral
//! instructions. The prompt is the agent's entire understanding of what it
//! needs to do when a session starts.

use std::fmt::Write;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Information needed to build a prompt.
pub struct PromptParams<'a> {
    /// Project system prompt contents (from workflow config file).
    pub system_prompt: Option<&'a str>,
    /// Issue/PR number (None for internal tasks).
    pub number: Option<u64>,
    /// Issue/PR title.
    pub title: &'a str,
    /// Issue/PR body.
    pub body: Option<&'a str>,
    /// Comment history — will be truncated to first 10 + last 10.
    pub comments: &'a [CommentInfo],
    /// Labels on the issue.
    pub labels: &'a [String],
    /// Assignees on the issue.
    pub assignees: &'a [String],
    /// Sub-issues for context.
    pub sub_issues: &'a [LinkedItemInfo],
    /// Linked PRs or issues for context.
    pub linked_items: &'a [LinkedItemInfo],
    /// Git branch name.
    pub branch: &'a str,
    /// Parent task info if this is a sub-task.
    pub parent: Option<&'a ParentInfo>,
    /// Other in-progress tasks for context.
    pub related_tasks: &'a [RelatedTaskInfo],
    /// Retry context if this is a retry.
    pub retry: Option<&'a RetryContext>,
    /// Orchestrator feedback from a previous PR rejection (issue #423).
    /// This is separate from retry context because it comes from orchestrator
    /// review, not from a session failure.
    pub rejection_feedback: Option<&'a str>,
}

/// A single comment on an issue or PR.
pub struct CommentInfo {
    pub author: String,
    pub timestamp: String,
    pub body: String,
}

/// A linked issue or PR (sub-issue, linked item).
pub struct LinkedItemInfo {
    pub number: u64,
    pub title: String,
    pub state: String,
}

/// Parent task reference for sub-tasks.
pub struct ParentInfo {
    pub number: u64,
    pub title: String,
}

/// Summary of a related in-progress task.
pub struct RelatedTaskInfo {
    pub number: u64,
    pub title: String,
    pub state: String,
}

/// Context provided when retrying a failed task (spec §15.2).
pub struct RetryContext {
    pub attempt: u32,
    pub previous_failure: String,
    pub has_prior_commits: bool,
    /// Detailed failure information from the previous session (spec §13.4).
    pub failure_details: Option<RetryFailureDetails>,
}

/// Detailed failure context for the retry prompt (spec §15.2).
pub struct RetryFailureDetails {
    /// Exit code from the previous session, if available.
    pub exit_code: Option<i32>,
    /// Signal that terminated the previous session, if any.
    pub signal: Option<String>,
    /// How long the previous session ran (seconds).
    pub duration_secs: u64,
    /// Failure type classification.
    pub failure_type: String,
    /// Human-readable failure summary.
    pub summary: String,
    /// Last lines of stderr from the previous session.
    pub stderr_tail: Vec<String>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of leading comments to include before truncation.
const HEAD_COMMENTS: usize = 10;
/// Maximum number of trailing comments to include before truncation.
const TAIL_COMMENTS: usize = 10;
/// Threshold above which comments are truncated.
const TRUNCATION_THRESHOLD: usize = HEAD_COMMENTS + TAIL_COMMENTS;

// ---------------------------------------------------------------------------
// GitHub comment conversion (spec §15.2)
// ---------------------------------------------------------------------------

/// Convert a GitHub comment to the prompt's `CommentInfo` format.
pub fn github_comment_to_info(comment: &tasks_github::model::Comment) -> CommentInfo {
    CommentInfo {
        author: comment.author.login.clone(),
        timestamp: comment.created_at.to_rfc3339(),
        body: comment.body.clone(),
    }
}

/// Fetch comments for a task from GitHub (spec §11, §15.2).
///
/// Returns an empty vec for internal tasks or on error (graceful degradation).
/// Errors are logged but do not prevent prompt construction.
pub async fn fetch_comments_for_task(
    client: &tasks_github::client::GitHubClient,
    source: &crate::model::task::TaskSource,
) -> Vec<CommentInfo> {
    use crate::model::task::TaskSource;

    match source {
        TaskSource::GithubIssue {
            owner,
            repo,
            number,
        } => match client.get_issue(owner, repo, *number).await {
            Ok(issue) => issue.comments.iter().map(github_comment_to_info).collect(),
            Err(e) => {
                tracing::warn!(
                    owner = %owner,
                    repo = %repo,
                    number = %number,
                    error = %e,
                    "failed to fetch issue comments for prompt"
                );
                Vec::new()
            }
        },
        TaskSource::GithubPr {
            owner,
            repo,
            number,
        } => match client.get_pull_request(owner, repo, *number).await {
            Ok(pr) => pr.comments.iter().map(github_comment_to_info).collect(),
            Err(e) => {
                tracing::warn!(
                    owner = %owner,
                    repo = %repo,
                    number = %number,
                    error = %e,
                    "failed to fetch PR comments for prompt"
                );
                Vec::new()
            }
        },
        TaskSource::Internal => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Build a complete agent prompt from task details (spec §15.3).
pub fn build_prompt(params: &PromptParams) -> String {
    let mut out = String::new();

    // 1. System prompt (project-level, optional) — spec §15.1 layer 1
    if let Some(system_prompt) = params.system_prompt {
        writeln!(out, "# Project Context\n").unwrap();
        writeln!(out, "{system_prompt}\n").unwrap();
    }

    // 2. Rejection feedback (issue #423) — prepended before retry/task
    // This is feedback from the orchestrator about why a previous PR was rejected.
    if let Some(feedback) = params.rejection_feedback {
        render_rejection_feedback(&mut out, feedback);
    }

    // 3. Retry context (spec §15.2) — prepended before task section
    if let Some(retry) = params.retry {
        render_retry(&mut out, retry);
    }

    // 4. Task description — spec §15.1 layer 2
    render_task(&mut out, params);

    // 5. Comments
    render_comments(&mut out, params.comments, params.number);

    // 6. Context — spec §15.1 layer 3
    render_context(&mut out, params);

    // 7. Behavioral instructions — spec §15.1 layer 4
    render_instructions(&mut out, params.branch, params.number);

    out
}

/// Build a prompt directly from a Task and branch name, with comments.
///
/// Extracts the issue/PR number from the task source and builds retry
/// context from the task's retry state. This keeps domain logic in the
/// server crate rather than in the app's run loop.
///
/// The `system_prompt` parameter should contain the loaded contents of the
/// project's system prompt file (from workflow.toml `[prompt].system_prompt`).
/// If the file doesn't exist or couldn't be loaded, pass `None`.
///
/// Comments should be fetched from GitHub at dispatch time using
/// [`fetch_comments_for_task`] and passed here.
pub fn build_prompt_for_task(
    task: &crate::model::task::Task,
    branch: &str,
    system_prompt: Option<&str>,
    comments: &[CommentInfo],
) -> String {
    let number = match &task.source {
        crate::model::task::TaskSource::GithubIssue { number, .. } => Some(*number),
        crate::model::task::TaskSource::GithubPr { number, .. } => Some(*number),
        crate::model::task::TaskSource::Internal => None,
    };

    let retry = if task.retry_count > 0 {
        // Build detailed failure context from last_failure if available (spec §13.4, §15.2)
        let failure_details = task.last_failure.as_ref().map(|f| RetryFailureDetails {
            exit_code: f.exit_code,
            signal: f.signal.clone(),
            duration_secs: f.duration_secs,
            failure_type: format!("{:?}", f.failure_type).to_lowercase(),
            summary: f.summary.clone(),
            stderr_tail: f.stderr_tail.clone(),
        });

        let previous_failure = task
            .last_failure
            .as_ref()
            .map(|f| f.summary.clone())
            .unwrap_or_else(|| "Previous session failed".to_string());

        Some(RetryContext {
            attempt: task.retry_count + 1,
            previous_failure,
            // Conservative: only claim prior commits exist after the first retry,
            // since the first attempt may have crashed before committing.
            has_prior_commits: task.retry_count > 1,
            failure_details,
        })
    } else {
        None
    };

    let params = PromptParams {
        system_prompt,
        number,
        title: &task.title,
        body: task.description.as_deref(),
        comments,
        labels: &task.labels,
        assignees: &[],
        sub_issues: &[],
        linked_items: &[],
        branch,
        parent: None,
        related_tasks: &[],
        retry: retry.as_ref(),
        rejection_feedback: task.rejection_feedback.as_deref(),
    };

    build_prompt(&params)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn render_rejection_feedback(out: &mut String, feedback: &str) {
    writeln!(out, "# Previous PR Rejection\n").unwrap();
    writeln!(
        out,
        "Your previous pull request was rejected by the orchestrator. Please address the following feedback:\n"
    )
    .unwrap();
    writeln!(out, "{feedback}\n").unwrap();
    writeln!(
        out,
        "Review this feedback carefully before starting work. The branch has been reset, so you are starting fresh.\n"
    )
    .unwrap();
}

fn render_retry(out: &mut String, retry: &RetryContext) {
    writeln!(out, "# Retry Information\n").unwrap();
    writeln!(
        out,
        "This is attempt {} (previous attempt failed).",
        retry.attempt
    )
    .unwrap();
    writeln!(out, "Previous failure: {}", retry.previous_failure).unwrap();
    if retry.has_prior_commits {
        writeln!(
            out,
            "Prior work exists on the branch — review existing commits before starting."
        )
        .unwrap();
    }
    writeln!(
        out,
        "Try a different approach if the previous one failed."
    )
    .unwrap();

    // Include detailed failure context if available (spec §13.4, §15.2)
    if let Some(details) = &retry.failure_details {
        writeln!(out).unwrap();
        writeln!(out, "## Previous Session Details\n").unwrap();
        writeln!(out, "- **Failure type**: {}", details.failure_type).unwrap();
        writeln!(out, "- **Duration**: {}s", details.duration_secs).unwrap();
        if let Some(code) = details.exit_code {
            writeln!(out, "- **Exit code**: {}", code).unwrap();
        }
        if let Some(ref signal) = details.signal {
            writeln!(out, "- **Signal**: {}", signal).unwrap();
        }

        // Include last few lines of stderr if available
        if !details.stderr_tail.is_empty() {
            writeln!(out).unwrap();
            writeln!(out, "### Last stderr output\n").unwrap();
            writeln!(out, "```").unwrap();
            // Show at most the last 10 lines to keep the prompt concise
            let lines_to_show = details.stderr_tail.len().min(10);
            for line in details.stderr_tail.iter().rev().take(lines_to_show).rev() {
                writeln!(out, "{}", line).unwrap();
            }
            writeln!(out, "```").unwrap();
        }
    }
    writeln!(out).unwrap();
}

fn render_task(out: &mut String, params: &PromptParams) {
    writeln!(out, "# Task\n").unwrap();
    if let Some(number) = params.number {
        writeln!(out, "**{}** (#{})\n", params.title, number).unwrap();
    } else {
        writeln!(out, "**{}**\n", params.title).unwrap();
    }
    if let Some(body) = params.body {
        writeln!(out, "{body}\n").unwrap();
    }
}

fn render_comments(out: &mut String, comments: &[CommentInfo], issue_number: Option<u64>) {
    if comments.is_empty() {
        return;
    }

    writeln!(out, "## Comments\n").unwrap();

    let total = comments.len();
    if total <= TRUNCATION_THRESHOLD {
        // Include all comments.
        for c in comments {
            render_single_comment(out, c);
        }
    } else {
        // First HEAD_COMMENTS
        for c in &comments[..HEAD_COMMENTS] {
            render_single_comment(out, c);
        }

        let omitted = total - HEAD_COMMENTS - TAIL_COMMENTS;
        if let Some(num) = issue_number {
            writeln!(
                out,
                "... ({omitted} comments omitted — use `gh issue view {num} --comments` for full history) ...\n"
            )
            .unwrap();
        } else {
            writeln!(out, "... ({omitted} comments omitted) ...\n").unwrap();
        }

        // Last TAIL_COMMENTS
        for c in &comments[total - TAIL_COMMENTS..] {
            render_single_comment(out, c);
        }
    }
}

fn render_single_comment(out: &mut String, c: &CommentInfo) {
    writeln!(out, "**{}** ({}):", c.author, c.timestamp).unwrap();
    writeln!(out, "{}\n", c.body).unwrap();
}

fn render_context(out: &mut String, params: &PromptParams) {
    writeln!(out, "## Context\n").unwrap();
    writeln!(out, "- Branch: `{}`", params.branch).unwrap();

    if let Some(parent) = params.parent {
        writeln!(out, "- Parent task: #{} — {}", parent.number, parent.title).unwrap();
    }

    if !params.related_tasks.is_empty() {
        let items: Vec<String> = params
            .related_tasks
            .iter()
            .map(|t| format!("#{} — {}", t.number, t.title))
            .collect();
        writeln!(out, "- Related in-progress tasks: {}", items.join(", ")).unwrap();
    }

    if !params.sub_issues.is_empty() {
        let items: Vec<String> = params
            .sub_issues
            .iter()
            .map(|i| format!("#{} — {} ({})", i.number, i.title, i.state))
            .collect();
        writeln!(out, "- Sub-issues: {}", items.join(", ")).unwrap();
    }

    if !params.linked_items.is_empty() {
        let items: Vec<String> = params
            .linked_items
            .iter()
            .map(|i| format!("#{} — {} ({})", i.number, i.title, i.state))
            .collect();
        writeln!(out, "- Linked: {}", items.join(", ")).unwrap();
    }

    if !params.labels.is_empty() {
        writeln!(out, "- Labels: {}", params.labels.join(", ")).unwrap();
    }

    writeln!(out).unwrap();
}

fn render_instructions(out: &mut String, branch: &str, issue_number: Option<u64>) {
    writeln!(out, "## Instructions\n").unwrap();
    writeln!(out, "- Work on the branch `{branch}`.").unwrap();
    writeln!(
        out,
        "- If you are stuck or the task is ambiguous, describe the problem clearly."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "### Valid outputs\n").unwrap();
    writeln!(out, "Your work can produce any of the following outputs:\n").unwrap();
    writeln!(out, "- **Code changes**: New features, bug fixes, refactoring, tests, or documentation committed to the branch.").unwrap();
    writeln!(
        out,
        "- **Pull requests**: Open a PR when code changes are ready for review."
    )
    .unwrap();
    writeln!(out, "- **Issue comments**: Progress updates, research findings, questions, or analysis posted to the issue.").unwrap();
    writeln!(out, "- **Issue updates**: Update the current issue with refined scope, implementation plans, or findings. Use `gh issue edit` to update the body or add labels.").unwrap();
    writeln!(out, "- **New issues**: Create issues for sub-tasks, bugs found, or follow-up work. Breaking a large task into smaller, well-scoped issues is a valuable output — sometimes the best way to \"complete\" a task is to decompose it.").unwrap();
    writeln!(out, "- **Plans or proposals**: Architecture decisions, implementation approaches, or design documents.").unwrap();
    writeln!(
        out,
        "- **Questions**: Ask for clarification when requirements are unclear or you need guidance."
    )
    .unwrap();
    writeln!(
        out,
        "- **Error reports**: If something is broken or blocked, describe what went wrong clearly."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "### Delivering your work\n").unwrap();
    writeln!(
        out,
        "When your task is finished, deliver your output using the GitHub CLI (`gh`)."
    )
    .unwrap();
    writeln!(out, "Choose the approach that fits what you produced:\n").unwrap();
    writeln!(
        out,
        "- **Code changes**: Commit your work, push the branch, and open a pull request \
         with `gh pr create`. Do not merge into main — the merge queue handles that. \
         The PR should reference the issue so it closes automatically when merged."
    )
    .unwrap();
    writeln!(
        out,
        "- **Research, analysis, or a question answer**: Comment your findings on the \
         issue with `gh issue comment`, then close it with `gh issue close`."
    )
    .unwrap();
    writeln!(
        out,
        "- **A plan or proposal that needs discussion**: Comment the plan on the issue \
         with `gh issue comment`. Leave the issue open for review."
    )
    .unwrap();
    writeln!(
        out,
        "- **Breaking down a large task**: Create new issues with `gh issue create` for \
         each sub-task. Reference the parent issue in each new issue body. Comment on the \
         original issue summarizing the breakdown, then close it. This is often the right \
         approach for ambiguous or large-scoped tasks."
    )
    .unwrap();
    writeln!(
        out,
        "- **Refining scope or adding context**: Update the issue with `gh issue edit` \
         to add implementation details, acceptance criteria, or scope clarifications. \
         Comment explaining what you learned and why you updated the issue."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(
        out,
        "Every task should end with a visible artifact on GitHub — a PR, an issue comment, \
         a new issue, or an issue update. If the task does not result in a PR, close the \
         issue yourself when the work is complete (unless leaving it open for discussion)."
    )
    .unwrap();

    if let Some(n) = issue_number {
        writeln!(out, "This task corresponds to issue #{n}.").unwrap();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build minimal params with sensible defaults.
    fn minimal_params<'a>() -> PromptParams<'a> {
        PromptParams {
            system_prompt: None,
            number: Some(42),
            title: "Implement widget",
            body: Some("Build the widget as described in the design doc."),
            comments: &[],
            labels: &[],
            assignees: &[],
            sub_issues: &[],
            linked_items: &[],
            branch: "tasks/42",
            parent: None,
            related_tasks: &[],
            retry: None,
            rejection_feedback: None,
        }
    }

    fn make_comments(n: usize) -> Vec<CommentInfo> {
        (0..n)
            .map(|i| CommentInfo {
                author: format!("user{i}"),
                timestamp: format!("2025-01-{:02}T12:00:00Z", (i % 28) + 1),
                body: format!("Comment body {i}"),
            })
            .collect()
    }

    #[test]
    fn basic_prompt_includes_essentials() {
        let params = minimal_params();
        let prompt = build_prompt(&params);

        assert!(prompt.contains("**Implement widget** (#42)"));
        assert!(prompt.contains("Build the widget as described in the design doc."));
        assert!(prompt.contains("Branch: `tasks/42`"));
        assert!(prompt.contains("Work on the branch `tasks/42`"));
        assert!(prompt.contains("gh pr create"));
        assert!(prompt.contains("gh issue comment"));
        assert!(prompt.contains("gh issue close"));
        assert!(prompt.contains("Do not merge into main"));
        assert!(prompt.contains("describe the problem clearly"));
        assert!(prompt.contains("issue #42"));

        // Valid outputs section
        assert!(prompt.contains("### Valid outputs"));
        assert!(prompt.contains("**Code changes**"));
        assert!(prompt.contains("**Pull requests**"));
        assert!(prompt.contains("**Issue comments**"));
        assert!(prompt.contains("**Issue updates**"));
        assert!(prompt.contains("**New issues**"));
        assert!(prompt.contains("**Plans or proposals**"));
        assert!(prompt.contains("**Questions**"));
        assert!(prompt.contains("**Error reports**"));
        // Issue operations are explicitly encouraged
        assert!(prompt.contains("gh issue edit"));
        assert!(prompt.contains("Breaking down a large task"));
    }

    #[test]
    fn comments_not_truncated_when_20_or_fewer() {
        let comments = make_comments(15);
        let params = PromptParams {
            comments: &comments,
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        // All 15 comments present.
        for i in 0..15 {
            assert!(
                prompt.contains(&format!("Comment body {i}")),
                "missing comment {i}"
            );
        }
        // No truncation note.
        assert!(!prompt.contains("comments omitted"));
    }

    #[test]
    fn comments_truncated_when_over_20() {
        let comments = make_comments(25);
        let params = PromptParams {
            comments: &comments,
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        // First 10 present.
        for i in 0..10 {
            assert!(
                prompt.contains(&format!("Comment body {i}")),
                "missing head comment {i}"
            );
        }
        // Last 10 present (indices 15..25).
        for i in 15..25 {
            assert!(
                prompt.contains(&format!("Comment body {i}")),
                "missing tail comment {i}"
            );
        }
        // Middle comments absent.
        for i in 10..15 {
            assert!(
                !prompt.contains(&format!("Comment body {i}")),
                "should be omitted: comment {i}"
            );
        }
        // Truncation note present with correct count.
        assert!(prompt.contains("5 comments omitted"));
        assert!(prompt.contains("gh issue view 42 --comments"));
    }

    #[test]
    fn system_prompt_prepended() {
        let params = PromptParams {
            system_prompt: Some("Use conventional commits. No semicolons."),
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        assert!(prompt.contains("# Project Context"));
        assert!(prompt.contains("Use conventional commits. No semicolons."));

        // System prompt appears before the task section.
        let ctx_pos = prompt.find("# Project Context").unwrap();
        let task_pos = prompt.find("# Task").unwrap();
        assert!(ctx_pos < task_pos);
    }

    #[test]
    fn retry_context_prepended() {
        let retry = RetryContext {
            attempt: 3,
            previous_failure: "Timeout after 600s".to_string(),
            has_prior_commits: true,
            failure_details: None,
        };
        let params = PromptParams {
            retry: Some(&retry),
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        assert!(prompt.contains("# Retry Information"));
        assert!(prompt.contains("attempt 3"));
        assert!(prompt.contains("Timeout after 600s"));
        assert!(prompt.contains("Prior work exists on the branch"));
        assert!(prompt.contains("Try a different approach"));

        // Retry info appears before the task section.
        let retry_pos = prompt.find("# Retry Information").unwrap();
        let task_pos = prompt.find("# Task").unwrap();
        assert!(retry_pos < task_pos);
    }

    #[test]
    fn retry_context_with_failure_details() {
        let retry = RetryContext {
            attempt: 2,
            previous_failure: "Process exited with code 1 (error)".to_string(),
            has_prior_commits: false,
            failure_details: Some(RetryFailureDetails {
                exit_code: Some(1),
                signal: None,
                duration_secs: 45,
                failure_type: "deterministic".to_string(),
                summary: "Process exited with code 1 (error)".to_string(),
                stderr_tail: vec![
                    "Error: Failed to compile".to_string(),
                    "  at src/main.rs:42".to_string(),
                ],
            }),
        };
        let params = PromptParams {
            retry: Some(&retry),
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        assert!(prompt.contains("## Previous Session Details"));
        assert!(prompt.contains("**Failure type**: deterministic"));
        assert!(prompt.contains("**Duration**: 45s"));
        assert!(prompt.contains("**Exit code**: 1"));
        assert!(prompt.contains("### Last stderr output"));
        assert!(prompt.contains("Error: Failed to compile"));
        assert!(prompt.contains("at src/main.rs:42"));
    }

    #[test]
    fn rejection_feedback_prepended() {
        let feedback = "The PR lacks test coverage for the new endpoint. Please add unit tests for the error cases.";
        let params = PromptParams {
            rejection_feedback: Some(feedback),
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        assert!(prompt.contains("# Previous PR Rejection"));
        assert!(prompt.contains("rejected by the orchestrator"));
        assert!(prompt.contains(feedback));
        assert!(prompt.contains("branch has been reset"));

        // Rejection feedback appears before the task section.
        let rejection_pos = prompt.find("# Previous PR Rejection").unwrap();
        let task_pos = prompt.find("# Task").unwrap();
        assert!(rejection_pos < task_pos);
    }

    #[test]
    fn rejection_feedback_with_retry_context() {
        let feedback = "Missing error handling for edge cases.";
        let retry = RetryContext {
            attempt: 2,
            previous_failure: "Session timeout".to_string(),
            has_prior_commits: false,
            failure_details: None,
        };
        let params = PromptParams {
            rejection_feedback: Some(feedback),
            retry: Some(&retry),
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        // Both sections should be present.
        assert!(prompt.contains("# Previous PR Rejection"));
        assert!(prompt.contains("# Retry Information"));

        // Rejection feedback comes before retry context.
        let rejection_pos = prompt.find("# Previous PR Rejection").unwrap();
        let retry_pos = prompt.find("# Retry Information").unwrap();
        let task_pos = prompt.find("# Task").unwrap();
        assert!(rejection_pos < retry_pos);
        assert!(retry_pos < task_pos);
    }

    #[test]
    fn empty_optionals_omitted() {
        let params = PromptParams {
            system_prompt: None,
            body: None,
            parent: None,
            related_tasks: &[],
            sub_issues: &[],
            linked_items: &[],
            labels: &[],
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        // No system prompt section.
        assert!(!prompt.contains("# Project Context"));
        // No rejection feedback section.
        assert!(!prompt.contains("# Previous PR Rejection"));
        // No parent line.
        assert!(!prompt.contains("Parent task"));
        // No related tasks line.
        assert!(!prompt.contains("Related in-progress tasks"));
        // No sub-issues line.
        assert!(!prompt.contains("Sub-issues"));
        // No linked line.
        assert!(!prompt.contains("Linked"));
        // No labels line.
        assert!(!prompt.contains("Labels"));
        // Comments section should not appear (no comments).
        assert!(!prompt.contains("## Comments"));
    }

    #[test]
    fn sub_issues_and_linked_items_rendered() {
        let sub_issues = vec![
            LinkedItemInfo {
                number: 100,
                title: "Sub-issue A".to_string(),
                state: "open".to_string(),
            },
            LinkedItemInfo {
                number: 101,
                title: "Sub-issue B".to_string(),
                state: "closed".to_string(),
            },
        ];
        let linked_items = vec![LinkedItemInfo {
            number: 200,
            title: "Related PR".to_string(),
            state: "merged".to_string(),
        }];
        let params = PromptParams {
            sub_issues: &sub_issues,
            linked_items: &linked_items,
            ..minimal_params()
        };
        let prompt = build_prompt(&params);

        assert!(
            prompt.contains("Sub-issues: #100 — Sub-issue A (open), #101 — Sub-issue B (closed)")
        );
        assert!(prompt.contains("Linked: #200 — Related PR (merged)"));
    }
}
