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

    // 2. Retry context (spec §15.2) — prepended before task section
    if let Some(retry) = params.retry {
        render_retry(&mut out, retry);
    }

    // 3. Task description — spec §15.1 layer 2
    render_task(&mut out, params);

    // 4. Comments
    render_comments(&mut out, params.comments, params.number);

    // 5. Context — spec §15.1 layer 3
    render_context(&mut out, params);

    // 6. Behavioral instructions — spec §15.1 layer 4
    render_instructions(&mut out, params.branch, params.number);

    out
}

/// Build a prompt directly from a Task and branch name.
///
/// Extracts the issue/PR number from the task source and builds retry
/// context from the task's retry state. This keeps domain logic in the
/// server crate rather than in the app's run loop.
pub fn build_prompt_for_task(task: &crate::model::task::Task, branch: &str) -> String {
    let number = match &task.source {
        crate::model::task::TaskSource::GithubIssue { number, .. } => Some(*number),
        crate::model::task::TaskSource::GithubPr { number, .. } => Some(*number),
        crate::model::task::TaskSource::Internal => None,
    };

    let retry = if task.retry_count > 0 {
        Some(RetryContext {
            attempt: task.retry_count + 1,
            previous_failure: "Previous session failed".to_string(),
            // Conservative: only claim prior commits exist after the first retry,
            // since the first attempt may have crashed before committing.
            has_prior_commits: task.retry_count > 1,
        })
    } else {
        None
    };

    let params = PromptParams {
        system_prompt: None, // TODO: load from workflow.toml
        number,
        title: &task.title,
        body: task.description.as_deref(),
        comments: &[],      // TODO: fetch from GitHub at dispatch time
        labels: &task.labels,
        assignees: &[],
        sub_issues: &[],
        linked_items: &[],
        branch,
        parent: None,
        related_tasks: &[],
        retry: retry.as_ref(),
    };

    build_prompt(&params)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

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
    writeln!(out, "Try a different approach if the previous one failed.\n").unwrap();
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
    writeln!(out, "- If you are stuck or the task is ambiguous, describe the problem clearly.").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "### Delivering your work\n").unwrap();
    writeln!(out, "When your task is finished, deliver your output using the GitHub CLI (`gh`).").unwrap();
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
        "- **New work items discovered**: Create new issues with `gh issue create` for \
         each item. Comment on the original issue summarizing what you filed, then close it."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(
        out,
        "Every task should end with a visible artifact on GitHub — a PR, an issue comment, \
         or a new issue. If the task does not result in a PR, close the issue yourself \
         when the work is complete."
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

        assert!(prompt.contains("Sub-issues: #100 — Sub-issue A (open), #101 — Sub-issue B (closed)"));
        assert!(prompt.contains("Linked: #200 — Related PR (merged)"));
    }
}
