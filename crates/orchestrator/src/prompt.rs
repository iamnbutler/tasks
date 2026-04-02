//! Prompt template for quality evaluation.
//!
//! Spec §7.3: The orchestrator evaluates PRs for merge worthiness by checking:
//! - Issue alignment: Does the change address the associated issue?
//! - Test status: Do tests pass (CI state)?
//! - Conflicts: Are there merge conflicts?
//! - Conventions: Does the change meet project standards?

use std::collections::HashSet;

use crate::types::QueueEntrySummary;
use tasks_github::model::{Issue, PullRequest, MergeableState, ReviewDecision, StatusCheckRollupState};

/// A file changed in a diff, with the line numbers that were modified.
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// File path (post-image, i.e. the "b/" side).
    pub path: String,
    /// Line numbers that were added or modified in the new version.
    pub changed_lines: Vec<usize>,
}

/// Parse a unified diff to extract changed files and their modified line numbers.
///
/// Returns files sorted by number of changed lines (most changes first).
pub fn parse_diff_files(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<usize> = Vec::new();
    // Current line number in the new file (post-image)
    let mut new_line: usize = 0;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            // Flush previous file
            if let Some(prev_path) = current_path.take() {
                if !current_lines.is_empty() {
                    files.push(DiffFile {
                        path: prev_path,
                        changed_lines: std::mem::take(&mut current_lines),
                    });
                }
            }
            current_path = Some(path.to_string());
        } else if line.starts_with("+++ /dev/null") {
            // File was deleted — skip
            if let Some(prev_path) = current_path.take() {
                if !current_lines.is_empty() {
                    files.push(DiffFile {
                        path: prev_path,
                        changed_lines: std::mem::take(&mut current_lines),
                    });
                }
            }
            current_path = None;
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some(new_start) = parse_hunk_new_start(line) {
                new_line = new_start;
            }
        } else if current_path.is_some() && !line.starts_with("diff ") && !line.starts_with("--- ") && !line.starts_with("index ") {
            if line.starts_with('+') {
                // Added line
                current_lines.push(new_line);
                new_line += 1;
            } else if line.starts_with('-') {
                // Removed line — don't advance new_line
            } else {
                // Context line
                new_line += 1;
            }
        }
    }

    // Flush last file
    if let Some(path) = current_path {
        if !current_lines.is_empty() {
            files.push(DiffFile {
                path,
                changed_lines: current_lines,
            });
        }
    }

    // Sort by most changed lines first
    files.sort_by(|a, b| b.changed_lines.len().cmp(&a.changed_lines.len()));
    files
}

/// Parse the new-file start line from a hunk header.
fn parse_hunk_new_start(line: &str) -> Option<usize> {
    // Format: @@ -old_start[,old_count] +new_start[,new_count] @@
    let plus_idx = line.find('+')?;
    let after_plus = &line[plus_idx + 1..];
    let end = after_plus.find(|c: char| c == ',' || c == ' ')?;
    after_plus[..end].parse().ok()
}

/// Build a context window around changed lines in a file.
///
/// Returns numbered lines (±`context` lines around each changed line),
/// with `...` markers where lines are skipped.
pub fn build_context_window(content: &str, changed_lines: &[usize], context: usize) -> String {
    if changed_lines.is_empty() {
        return String::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Build set of line numbers to include (1-indexed)
    let include: HashSet<usize> = changed_lines
        .iter()
        .flat_map(|&line| {
            let start = line.saturating_sub(context).max(1);
            let end = (line + context).min(total);
            start..=end
        })
        .collect();

    let mut result = String::new();
    let mut prev_included = false;

    for (idx, line_content) in lines.iter().enumerate() {
        let line_num = idx + 1; // 1-indexed
        if include.contains(&line_num) {
            if !prev_included && line_num > 1 && !result.is_empty() {
                result.push_str("...\n");
            }
            result.push_str(&format!("{}: {}\n", line_num, line_content));
            prev_included = true;
        } else {
            prev_included = false;
        }
    }

    if !prev_included && !result.is_empty() {
        result.push_str("...\n");
    }

    result
}

/// Build the system prompt for quality evaluation.
pub fn system_prompt() -> String {
    r#"You are a code review orchestrator evaluating whether a pull request is ready to merge.

You review the ACTUAL CODE DIFF, not just PR metadata. Do not trust the PR description or any self-reported "test plan" — these are written by the same agent that wrote the code and may be boilerplate.

## Review process

**Pass 1 — Diff triage:**
Read the diff carefully. You may also receive a "File Context" section with surrounding code for the most-changed files — use this to understand how changes integrate with the rest of the codebase.

Evaluate:

1. **Issue alignment**: Does the diff actually address the issue? Not "does the PR description say it does" — do the actual code changes solve the problem?
2. **Correctness**: Are there obvious bugs, missing error handling, or incomplete changes? For removals: is anything left that depends on the removed code? For additions: does the new code handle edge cases? Use file context (when available) to check callers, imports, and surrounding logic.
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
///
/// `file_contexts` provides proactively-fetched context windows for key files
/// in the diff (path, windowed content). When non-empty, these are included
/// so the reviewer can see surrounding code, not just isolated hunks.
pub fn build_evaluation_prompt(
    pr: &PullRequest,
    issue: Option<&Issue>,
    task_title: &str,
    task_description: Option<&str>,
    diff: Option<&str>,
    queue_context: &[QueueEntrySummary],
) -> String {
    build_evaluation_prompt_with_context(pr, issue, task_title, task_description, diff, queue_context, &[])
}

/// Build the user prompt with PR and issue context, including proactive file context.
pub fn build_evaluation_prompt_with_context(
    pr: &PullRequest,
    issue: Option<&Issue>,
    task_title: &str,
    task_description: Option<&str>,
    diff: Option<&str>,
    queue_context: &[QueueEntrySummary],
    file_contexts: &[(String, String)],
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

    // CI status - critical for merge decision
    match pr.ci_status {
        Some(StatusCheckRollupState::Success) => {
            prompt.push_str("- **CI Status**: ✓ All checks passing\n");
        }
        Some(StatusCheckRollupState::Pending) => {
            prompt.push_str("- **CI Status**: ⏳ Checks still running\n");
        }
        Some(StatusCheckRollupState::Failure) => {
            prompt.push_str("- **CI Status**: ✗ FAILING — DO NOT APPROVE\n");
            // Include failed check details
            let failed: Vec<_> = pr
                .check_runs
                .iter()
                .filter(|c| c.conclusion.as_deref() == Some("failure"))
                .collect();
            if !failed.is_empty() {
                prompt.push_str("  - Failed checks:\n");
                for check in failed.iter().take(5) {
                    prompt.push_str(&format!("    - {}\n", check.name));
                }
                if failed.len() > 5 {
                    prompt.push_str(&format!("    - ... and {} more\n", failed.len() - 5));
                }
            }
        }
        Some(StatusCheckRollupState::Error) => {
            prompt.push_str("- **CI Status**: ⚠ Error — checks could not run\n");
        }
        Some(StatusCheckRollupState::Expected) => {
            prompt.push_str("- **CI Status**: Expected (checks not yet started)\n");
        }
        None => {
            prompt.push_str("- **CI Status**: Unknown (no status checks configured)\n");
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

    // Proactive file context — surrounding code for key changed files
    if !file_contexts.is_empty() {
        prompt.push_str("## File Context\n\n");
        prompt.push_str("Surrounding code context for the most-changed files (line numbers shown):\n\n");
        for (path, content) in file_contexts {
            prompt.push_str(&format!("### `{}`\n\n", path));
            prompt.push_str("```\n");
            prompt.push_str(&truncate_text(content, 5_000));
            prompt.push_str("\n```\n\n");
        }
    }

    prompt.push_str("## Evaluation Request\n\n");
    prompt.push_str("Review the diff above against the issue requirements. ");
    if !file_contexts.is_empty() {
        prompt.push_str("Use the file context to understand how changes integrate with surrounding code. ");
    }
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
    // Start with the same base prompt (no file_contexts — deep review has its own files section)
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
            ci_status: None,
            check_runs: vec![],
            status_contexts: vec![],
            latest_reviews: vec![],
            reaction_count: 0,
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
    fn test_parse_diff_files_basic() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc123..def456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,4 @@ fn main() {
     let x = 1;
     let y = 2;
+    let z = 3;
     println!("done");
"#;
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].changed_lines, vec![12]);
    }

    #[test]
    fn test_parse_diff_files_multiple() {
        let diff = r#"diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,5 @@
 line1
+added1
+added2
 line3
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -5,3 +5,4 @@
 ctx
+new_line
 ctx
"#;
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 2);
        // a.rs has more changes, should be first
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].changed_lines.len(), 2);
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[1].changed_lines.len(), 1);
    }

    #[test]
    fn test_parse_diff_files_deleted_file() {
        let diff = r#"diff --git a/src/old.rs b/src/old.rs
--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-line1
-line2
-line3
"#;
        let files = parse_diff_files(diff);
        // Deleted files have no added lines
        assert!(files.is_empty());
    }

    #[test]
    fn test_build_context_window_basic() {
        let content = (1..=20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = build_context_window(&content, &[10], 3);
        // Should include lines 7-13
        assert!(result.contains("7: line 7"));
        assert!(result.contains("10: line 10"));
        assert!(result.contains("13: line 13"));
        // Should start with line 7 (not line 1)
        assert!(result.starts_with("7: "), "should start at line 7, got:\n{result}");
    }

    #[test]
    fn test_build_context_window_multiple_regions() {
        let content = (1..=100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = build_context_window(&content, &[10, 90], 3);
        // Should have two regions with ... separator
        assert!(result.contains("10: line 10"));
        assert!(result.contains("90: line 90"));
        assert!(result.contains("..."));
    }

    #[test]
    fn test_build_context_window_empty_lines() {
        let result = build_context_window("some content", &[], 50);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_evaluation_prompt_with_file_context() {
        let pr = test_pr();
        let file_contexts = vec![
            ("src/auth.rs".to_string(), "10: fn login() {\n11:     // impl\n12: }".to_string()),
        ];
        let prompt = build_evaluation_prompt_with_context(
            &pr,
            None,
            "Fix auth bug",
            None,
            Some("diff content"),
            &[],
            &file_contexts,
        );

        assert!(prompt.contains("## File Context"));
        assert!(prompt.contains("src/auth.rs"));
        assert!(prompt.contains("fn login()"));
        assert!(prompt.contains("Use the file context"));
    }

    #[test]
    fn test_build_evaluation_prompt_without_file_context_unchanged() {
        let pr = test_pr();
        let with_context = build_evaluation_prompt_with_context(
            &pr, None, "Test", None, None, &[], &[],
        );
        let without_context = build_evaluation_prompt(
            &pr, None, "Test", None, None, &[],
        );
        assert_eq!(with_context, without_context);
        assert!(!with_context.contains("## File Context"));
    }

    #[test]
    fn test_system_prompt_mentions_file_context() {
        let prompt = system_prompt();
        assert!(prompt.contains("File Context"));
    }
}
