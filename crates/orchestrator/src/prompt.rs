//! Prompt template for quality evaluation.
//!
//! Spec §7.3: The orchestrator evaluates PRs for merge worthiness by checking:
//! - Issue alignment: Does the change address the associated issue?
//! - Test status: Do tests pass (CI state)?
//! - Conflicts: Are there merge conflicts?
//! - Conventions: Does the change meet project standards?

use crate::types::QueueEntrySummary;
use tasks_github::model::{Issue, PullRequest, MergeableState, ReviewDecision};

/// Build the system prompt for quality evaluation.
pub fn system_prompt() -> String {
    r#"You are a code review orchestrator evaluating whether a pull request is ready to merge.

You review the ACTUAL CODE DIFF, not just PR metadata. Do not trust the PR description or any self-reported "test plan" — these are written by the same agent that wrote the code and may be boilerplate.

## Review process

**Pass 1 — Diff triage:**
Read the diff carefully. Evaluate:

1. **Issue alignment**: Does the diff actually address the issue? Not "does the PR description say it does" — do the actual code changes solve the problem?
2. **Correctness**: Are there obvious bugs, missing error handling, or incomplete changes? For removals: is anything left that depends on the removed code? For additions: does the new code handle edge cases?
3. **Completeness**: Does the diff cover all aspects of the issue, or are there gaps?
4. **Conflicts/CI**: Check mergeable state and review status from the metadata.
5. **Queue context**: Consider other PRs in the merge queue. If this PR appears to depend on changes from another PR that hasn't merged yet, or if issues you see would be resolved by a PR ahead in the queue, factor that into your decision.

After reading the diff, decide:
- If the change is obviously correct and complete → approve or reject immediately
- If you're unsure or the change is substantial → request deeper review
- If the PR depends on another queued PR → approve with a note, or hold feedback until dependencies merge

**Response format for Pass 1 (JSON):**
{
  "approved": true|false,
  "needs_deeper_review": false,
  "reasoning": "Explanation based on what you see in the diff",
  "feedback": "Specific feedback if rejected, or null if approved",
  "files_to_review": null
}

If you need deeper review:
{
  "approved": false,
  "needs_deeper_review": true,
  "reasoning": "What concerns you and why you need more context",
  "feedback": null,
  "files_to_review": ["path/to/file1.rs", "path/to/file2.rs"]
}

Request at most 5 files. Pick the ones most relevant to your concern.

**Pass 2 — Deep review (if requested):**
You'll receive the full content of the files you requested. Now evaluate with full context:
- Does the change integrate correctly with the surrounding code?
- Are there callers/dependents of removed or changed code that weren't updated?
- Is the implementation approach reasonable?

Response format for Pass 2 is the same, but `needs_deeper_review` must be false.

## Key principles

- Do not trust self-reported test plans. The agent saying "I ran cargo build" is not verification.
- Look at what the diff actually does, not what the PR says it does.
- For feature removals: check if anything in the diff's context still references the removed feature.
- For large diffs: if truncated, lean toward requesting deeper review rather than approving blind.
- Be concise but specific. If rejecting, point to exact lines/hunks in the diff.
- Consider queue ordering: if a "missing function" or "undefined reference" issue might be resolved by a PR ahead in the queue, mention this in your reasoning rather than rejecting outright."#.to_string()
}

/// Build the user prompt with PR and issue context.
pub fn build_evaluation_prompt(
    pr: &PullRequest,
    issue: Option<&Issue>,
    task_title: &str,
    task_description: Option<&str>,
    diff: Option<&str>,
    queue_context: &[QueueEntrySummary],
) -> String {
    let mut prompt = String::new();

    // Task context
    prompt.push_str("## Task\n\n");
    prompt.push_str(&format!("**Title**: {}\n", task_title));
    if let Some(desc) = task_description {
        prompt.push_str(&format!("**Description**: {}\n", desc));
    }
    prompt.push('\n');

    // Issue context (if available)
    if let Some(issue) = issue {
        prompt.push_str("## Associated Issue\n\n");
        prompt.push_str(&format!("**Issue #{}: {}**\n\n", issue.number, issue.title));
        if let Some(body) = &issue.body {
            prompt.push_str(&format!("{}\n\n", truncate_text(body, 2000)));
        }

        // Include recent issue comments for context
        if !issue.comments.is_empty() {
            prompt.push_str("### Recent Issue Comments\n\n");
            for comment in issue.comments.iter().take(5) {
                prompt.push_str(&format!(
                    "**@{}**: {}\n\n",
                    comment.author.login,
                    truncate_text(&comment.body, 500)
                ));
            }
        }
    }

    // PR metadata (no body — we don't trust the agent's self-reported description)
    prompt.push_str("## Pull Request Metadata\n\n");
    prompt.push_str(&format!("**PR #{}: {}**\n\n", pr.number, pr.title));
    prompt.push_str(&format!("- **Branch**: {} -> {}\n", pr.head_ref, pr.base_ref));
    prompt.push_str(&format!("- **Author**: @{}\n", pr.author.login));
    prompt.push_str(&format!("- **State**: {:?}\n", pr.state));
    prompt.push_str(&format!("- **Draft**: {}\n", pr.is_draft));

    // Mergeable state
    match pr.mergeable {
        Some(MergeableState::Mergeable) => {
            prompt.push_str("- **Mergeable**: Yes\n");
        }
        Some(MergeableState::Conflicting) => {
            prompt.push_str("- **Mergeable**: No (has conflicts)\n");
        }
        Some(MergeableState::Unknown) | None => {
            prompt.push_str("- **Mergeable**: Unknown\n");
        }
    }

    // Review decision
    match pr.review_decision {
        Some(ReviewDecision::Approved) => {
            prompt.push_str("- **Review Status**: Approved\n");
        }
        Some(ReviewDecision::ChangesRequested) => {
            prompt.push_str("- **Review Status**: Changes requested\n");
        }
        Some(ReviewDecision::ReviewRequired) => {
            prompt.push_str("- **Review Status**: Review required\n");
        }
        None => {
            prompt.push_str("- **Review Status**: No required reviews\n");
        }
    }

    prompt.push('\n');

    // Reviews
    if !pr.reviews.is_empty() {
        prompt.push_str("### Reviews\n\n");
        for review in &pr.reviews {
            prompt.push_str(&format!(
                "- **@{}** ({:?})",
                review.author.login, review.state
            ));
            if let Some(body) = &review.body {
                if !body.is_empty() {
                    prompt.push_str(&format!(": {}", truncate_text(body, 300)));
                }
            }
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    // Linked issues
    if !pr.linked_issues.is_empty() {
        prompt.push_str("### Linked Issues\n\n");
        for linked in &pr.linked_issues {
            prompt.push_str(&format!(
                "- #{} {} ({:?})\n",
                linked.number, linked.title, linked.state
            ));
        }
        prompt.push('\n');
    }

    // Queue context — other PRs in the merge queue
    if !queue_context.is_empty() {
        prompt.push_str("## Merge Queue Context\n\n");
        prompt.push_str("Other PRs currently in the merge queue (ordered by position):\n\n");
        for entry in queue_context {
            let position = entry
                .queue_position
                .map(|p| format!("#{}", p))
                .unwrap_or_else(|| "-".to_string());
            prompt.push_str(&format!(
                "- **PR #{}**: {} (status: {:?}, position: {})\n",
                entry.pr_number, entry.task_title, entry.status, position
            ));
        }
        prompt.push_str("\nConsider whether this PR might depend on or conflict with any of the above.\n\n");
    }

    // The diff — this is what the review is actually about
    if let Some(diff) = diff {
        prompt.push_str("## Diff\n\n");
        prompt.push_str("```diff\n");
        prompt.push_str(diff);
        if !diff.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("```\n\n");
    } else {
        prompt.push_str("## Diff\n\n");
        prompt.push_str("**No diff available.** Evaluate based on metadata only, and lean toward requesting deeper review.\n\n");
    }

    prompt.push_str("## Evaluation Request\n\n");
    prompt.push_str("Review the diff above against the issue requirements. ");
    prompt.push_str("Evaluate correctness, completeness, and whether this actually solves the issue. ");
    prompt.push_str("Respond with your evaluation in the JSON format specified in your instructions.");

    prompt
}

/// Build the follow-up prompt for pass 2 (deep review).
///
/// Includes the original context plus the requested file contents.
pub fn build_deep_review_prompt(
    pr: &PullRequest,
    issue: Option<&Issue>,
    task_title: &str,
    task_description: Option<&str>,
    diff: &str,
    review_reasoning: &str,
    files: &[(String, String)],
    queue_context: &[QueueEntrySummary],
) -> String {
    // Start with the same base prompt
    let mut prompt = build_evaluation_prompt(pr, issue, task_title, task_description, Some(diff), queue_context);

    // Add the reviewer's reasoning from pass 1
    prompt.push_str("\n## Previous Review Notes\n\n");
    prompt.push_str(review_reasoning);
    prompt.push_str("\n\n");

    // Add requested file contents
    prompt.push_str("## Requested Files\n\n");
    for (path, content) in files {
        prompt.push_str(&format!("### `{}`\n\n", path));
        prompt.push_str("```\n");
        prompt.push_str(&truncate_text(content, 10_000));
        prompt.push_str("\n```\n\n");
    }

    prompt.push_str("## Deep Review Request\n\n");
    prompt.push_str("You now have the file context you requested. ");
    prompt.push_str("Make your final evaluation — approve or reject. ");
    prompt.push_str("You cannot request more files. Respond with your evaluation JSON.");

    prompt
}

/// Truncate text to a maximum length, adding ellipsis if truncated.
fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", &text[..max_len.saturating_sub(3)])
    }
}

/// Parse a GitHub PR URL into (owner, repo, number).
///
/// Accepts URLs like:
/// - https://github.com/owner/repo/pull/123
/// - http://github.com/owner/repo/pull/123
pub fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    // Strip the protocol
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;

    // Split into parts: owner/repo/pull/number
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || parts[2] != "pull" {
        return None;
    }

    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let number = parts[3].parse().ok()?;

    Some((owner, repo, number))
}

/// Build the system prompt for triage and decomposition.
pub fn triage_system_prompt() -> String {
    r#"You are a project foreman decomposing work into well-formed GitHub issues.

## Your Role

The human has described work in natural language. Your job is to:
1. Analyze the request and understand its scope
2. Decompose it into concrete, actionable issues
3. Define clear acceptance criteria for each issue
4. Identify dependencies between issues
5. Reference relevant code when possible

## Output Format

Respond with JSON:
```json
{
  "analysis": "Brief analysis of the request — what it entails, key considerations, estimated complexity",
  "issues": [
    {
      "title": "Short, actionable title (imperative mood, e.g., 'Add rate limiting middleware')",
      "body": "Markdown body with:\n## Context\nWhy this is needed.\n\n## Requirements\n- Bullet list of specific requirements\n\n## Acceptance Criteria\n- [ ] Checkboxes for done criteria\n\n## Notes\nRelevant code paths, files, or architectural considerations.",
      "labels": ["enhancement"],
      "blocked_by": []
    }
  ]
}
```

## Decomposition Principles

- **Right-sized issues**: Each issue should be completable by a single agent session (a few hours of focused work). If something is too big, split it.
- **Clear boundaries**: Each issue should have a clear scope — no ambiguity about what "done" looks like.
- **Dependency ordering**: Use `blocked_by` to reference other issues in the batch by their zero-based index. Issue 0 is the first in the array. Only add dependencies when there's a true ordering constraint (e.g., API must exist before UI can call it).
- **No unnecessary decomposition**: If the work is simple and fits in one issue, return one issue. Don't split for the sake of splitting.
- **Actionable titles**: Use imperative mood ("Add X", "Fix Y", "Refactor Z"), not descriptions ("X should be added").
- **Reference code**: When you know which files, modules, or functions are involved, mention them in the issue body.
- **Labels**: Use standard labels — "enhancement", "bug", "refactor", "documentation", "testing". Only include labels that genuinely apply.

## Existing Issues

You'll receive a list of existing open issues for context. Avoid creating duplicates. If an existing issue already covers part of the request, reference it instead of creating a new one.

## Key Rules

- Respond ONLY with the JSON object — no prose before or after.
- The `blocked_by` field uses zero-based indices into the `issues` array.
- Every issue must have a non-empty title and body.
- Keep analysis concise (2-4 sentences)."#.to_string()
}

/// Build the user prompt for triage decomposition.
pub fn build_triage_prompt(
    description: &str,
    repo: &str,
    existing_issues: &[crate::types::TriageIssueSummary],
) -> String {
    let mut prompt = format!(
        "## Work Request\n\n\
         **Repository:** {repo}\n\n\
         {description}\n\n"
    );

    if !existing_issues.is_empty() {
        prompt.push_str("## Existing Open Issues\n\n");
        for issue in existing_issues.iter().take(30) {
            let labels = if issue.labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", issue.labels.join(", "))
            };
            prompt.push_str(&format!("- #{}: {}{}\n", issue.number, issue.title, labels));
        }
        prompt.push('\n');
    }

    prompt.push_str("## Instructions\n\n\
         Decompose the work request above into well-formed GitHub issues. \
         Consider the existing issues to avoid duplicates. \
         Respond with your JSON decomposition.");

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_github::model::{
        Comment, Issue, IssueState, Label, PullRequest, PullRequestState, Review,
        ReviewDecision, ReviewState, User,
    };
    use chrono::Utc;

    fn test_user() -> User {
        User {
            login: "testuser".to_string(),
            node_id: "node-1".to_string(),
        }
    }

    fn test_pr() -> PullRequest {
        PullRequest {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 42,
            node_id: "PR_123".to_string(),
            title: "Fix authentication bug".to_string(),
            body: Some("This PR fixes the login timeout issue.".to_string()),
            state: PullRequestState::Open,
            head_ref: "fix/auth-bug".to_string(),
            head_sha: "abc123".to_string(),
            base_ref: "main".to_string(),
            is_draft: false,
            mergeable: Some(MergeableState::Mergeable),
            labels: vec![Label {
                name: "bug".to_string(),
                color: "ff0000".to_string(),
            }],
            assignees: vec![],
            review_decision: Some(ReviewDecision::Approved),
            reviews: vec![Review {
                id: "review-1".to_string(),
                author: test_user(),
                state: ReviewState::Approved,
                body: Some("LGTM".to_string()),
                submitted_at: Utc::now(),
            }],
            comments: vec![],
            linked_issues: vec![],
            author: test_user(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
            merged_at: None,
        }
    }

    fn test_issue() -> Issue {
        Issue {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 10,
            node_id: "I_123".to_string(),
            title: "Authentication timeout".to_string(),
            body: Some("Users are experiencing login timeouts after 30 seconds.".to_string()),
            state: IssueState::Open,
            state_reason: None,
            labels: vec![],
            assignees: vec![],
            milestone: None,
            comments: vec![Comment {
                id: "comment-1".to_string(),
                author: test_user(),
                body: "Can reproduce on Chrome".to_string(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            parent: None,
            sub_issues: vec![],
            blocked_by: vec![],
            linked_pull_requests: vec![],
            author: test_user(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
        }
    }

    #[test]
    fn test_parse_pr_url_https() {
        let result = parse_pr_url("https://github.com/owner/repo/pull/123");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string(), 123)));
    }

    #[test]
    fn test_parse_pr_url_http() {
        let result = parse_pr_url("http://github.com/owner/repo/pull/42");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string(), 42)));
    }

    #[test]
    fn test_parse_pr_url_invalid() {
        assert_eq!(parse_pr_url("https://github.com/owner/repo"), None);
        assert_eq!(parse_pr_url("https://github.com/owner/repo/issues/1"), None);
        assert_eq!(parse_pr_url("not a url"), None);
    }

    #[test]
    fn test_truncate_text_short() {
        assert_eq!(truncate_text("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_text_long() {
        let long = "a".repeat(100);
        let truncated = truncate_text(&long, 50);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.len(), 50);
    }

    #[test]
    fn test_system_prompt_contains_criteria() {
        let prompt = system_prompt();
        assert!(prompt.contains("Issue alignment"));
        assert!(prompt.contains("Correctness"));
        assert!(prompt.contains("Completeness"));
        assert!(prompt.contains("Conflicts/CI"));
        assert!(prompt.contains("JSON"));
    }

    #[test]
    fn test_build_evaluation_prompt_basic() {
        let pr = test_pr();
        let prompt = build_evaluation_prompt(&pr, None, "Fix auth bug", None, None, &[]);

        // Should contain task info
        assert!(prompt.contains("Fix auth bug"));

        // Should contain PR info
        assert!(prompt.contains("PR #42"));
        assert!(prompt.contains("Fix authentication bug"));
        assert!(prompt.contains("fix/auth-bug"));
        assert!(prompt.contains("main"));

        // Should contain mergeable state
        assert!(prompt.contains("Mergeable"));

        // Should contain review status
        assert!(prompt.contains("Approved"));
    }

    #[test]
    fn test_build_evaluation_prompt_with_issue() {
        let pr = test_pr();
        let issue = test_issue();
        let prompt = build_evaluation_prompt(
            &pr,
            Some(&issue),
            "Fix auth bug",
            Some("Fix the timeout issue"),
            None,
            &[],
        );

        // Should contain issue info
        assert!(prompt.contains("Issue #10"));
        assert!(prompt.contains("Authentication timeout"));
        assert!(prompt.contains("login timeouts"));

        // Should contain issue comments
        assert!(prompt.contains("Can reproduce on Chrome"));
    }

    #[test]
    fn test_build_evaluation_prompt_with_conflicts() {
        let mut pr = test_pr();
        pr.mergeable = Some(MergeableState::Conflicting);

        let prompt = build_evaluation_prompt(&pr, None, "Test", None, None, &[]);
        assert!(prompt.contains("has conflicts"));
    }

    #[test]
    fn test_build_evaluation_prompt_changes_requested() {
        let mut pr = test_pr();
        pr.review_decision = Some(ReviewDecision::ChangesRequested);

        let prompt = build_evaluation_prompt(&pr, None, "Test", None, None, &[]);
        assert!(prompt.contains("Changes requested"));
    }

    #[test]
    fn test_build_evaluation_prompt_with_reviews() {
        let pr = test_pr();
        let prompt = build_evaluation_prompt(&pr, None, "Test", None, None, &[]);

        // Should contain review info
        assert!(prompt.contains("@testuser"));
        assert!(prompt.contains("LGTM"));
    }

    #[test]
    fn test_system_prompt_is_skeptical() {
        let prompt = system_prompt();
        assert!(prompt.contains("diff"));
        assert!(prompt.contains("Do not trust"));
        assert!(prompt.contains("needs_deeper_review"));
    }

    #[test]
    fn test_build_evaluation_prompt_includes_diff() {
        let pr = test_pr();
        let prompt = build_evaluation_prompt(
            &pr,
            None,
            "Fix auth bug",
            None,
            Some("diff --git a/src/auth.rs b/src/auth.rs\n-old\n+new"),
            &[],
        );
        assert!(prompt.contains("## Diff"));
        assert!(prompt.contains("-old"));
        assert!(prompt.contains("+new"));
    }

    #[test]
    fn test_build_evaluation_prompt_no_diff_notes_absence() {
        let pr = test_pr();
        let prompt = build_evaluation_prompt(&pr, None, "Test", None, None, &[]);
        assert!(prompt.contains("No diff available"));
    }

    #[test]
    fn test_build_deep_review_prompt() {
        let pr = test_pr();
        let files = vec![
            ("src/auth.rs".to_string(), "fn login() { todo!() }".to_string()),
        ];
        let prompt = build_deep_review_prompt(
            &pr,
            None,
            "Fix auth bug",
            None,
            "diff content here",
            "I need to see src/auth.rs to verify the login flow is complete",
            &files,
            &[],
        );
        assert!(prompt.contains("## Requested Files"));
        assert!(prompt.contains("src/auth.rs"));
        assert!(prompt.contains("fn login()"));
        assert!(prompt.contains("Previous Review Notes"));
    }

    #[test]
    fn test_build_evaluation_prompt_with_queue_context() {
        use crate::types::QueueEntrySummary;
        use models::merge_queue::MergeStatus;
        use chrono::Utc;

        let pr = test_pr();
        let queue_context = vec![
            QueueEntrySummary {
                pr_url: "https://github.com/owner/repo/pull/40".to_string(),
                pr_number: 40,
                task_title: "Add logging feature".to_string(),
                status: MergeStatus::Approved,
                queued_at: Utc::now(),
                queue_position: Some(1),
            },
            QueueEntrySummary {
                pr_url: "https://github.com/owner/repo/pull/41".to_string(),
                pr_number: 41,
                task_title: "Fix database connection".to_string(),
                status: MergeStatus::Pending,
                queued_at: Utc::now(),
                queue_position: Some(2),
            },
        ];

        let prompt = build_evaluation_prompt(&pr, None, "Test task", None, None, &queue_context);

        // Should contain queue context section
        assert!(prompt.contains("## Merge Queue Context"));
        assert!(prompt.contains("PR #40"));
        assert!(prompt.contains("Add logging feature"));
        assert!(prompt.contains("PR #41"));
        assert!(prompt.contains("Fix database connection"));
        assert!(prompt.contains("Consider whether this PR might depend on"));
    }

    #[test]
    fn test_system_prompt_mentions_queue_context() {
        let prompt = system_prompt();
        assert!(prompt.contains("Queue context"));
        assert!(prompt.contains("queue ordering"));
    }

    #[test]
    fn test_triage_system_prompt_contains_key_instructions() {
        let prompt = triage_system_prompt();
        assert!(prompt.contains("decompos"));
        assert!(prompt.contains("blocked_by"));
        assert!(prompt.contains("acceptance criteria"));
        assert!(prompt.contains("JSON"));
        assert!(prompt.contains("zero-based"));
    }

    #[test]
    fn test_build_triage_prompt_basic() {
        let prompt = build_triage_prompt(
            "Add rate limiting to the API",
            "owner/repo",
            &[],
        );
        assert!(prompt.contains("Add rate limiting to the API"));
        assert!(prompt.contains("owner/repo"));
        assert!(prompt.contains("Decompose"));
    }

    #[test]
    fn test_build_triage_prompt_with_existing_issues() {
        use crate::types::TriageIssueSummary;

        let existing = vec![
            TriageIssueSummary {
                number: 10,
                title: "Add auth middleware".to_string(),
                labels: vec!["enhancement".to_string()],
            },
            TriageIssueSummary {
                number: 11,
                title: "Fix rate limit header".to_string(),
                labels: vec!["bug".to_string()],
            },
        ];

        let prompt = build_triage_prompt(
            "Add rate limiting to the API",
            "owner/repo",
            &existing,
        );
        assert!(prompt.contains("#10: Add auth middleware"));
        assert!(prompt.contains("[enhancement]"));
        assert!(prompt.contains("#11: Fix rate limit header"));
        assert!(prompt.contains("[bug]"));
        assert!(prompt.contains("avoid duplicates"));
    }
}
