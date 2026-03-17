//! Prompt template for quality evaluation.
//!
//! Spec §7.3: The orchestrator evaluates PRs for merge worthiness by checking:
//! - Issue alignment: Does the change address the associated issue?
//! - Test status: Do tests pass (CI state)?
//! - Conflicts: Are there merge conflicts?
//! - Conventions: Does the change meet project standards?

use tasks_github::model::{Issue, PullRequest, MergeableState, ReviewDecision};

/// Build the system prompt for quality evaluation.
pub fn system_prompt() -> String {
    r#"You are a code review orchestrator evaluating whether a pull request is ready to merge.

Your job is to assess merge worthiness based on these criteria (spec §7.3):

1. **Issue Alignment**: Does the change address the associated issue as described?
   - Compare the PR's changes to the issue's requirements
   - Check if the implementation matches what was requested
   - Note any missing functionality or scope creep

2. **Test/CI Status**: Do tests pass?
   - Check the CI status from GitHub
   - Flag any failing checks or pending workflows

3. **Merge Conflicts**: Are there conflicts that need resolution?
   - Check the mergeable state
   - A PR with conflicts cannot be approved

4. **Project Conventions**: Does the change meet quality standards?
   - Review decisions from human reviewers
   - Check if required reviews are present
   - Note any requested changes

After analysis, respond with a JSON object in exactly this format:
{
  "approved": true|false,
  "reasoning": "A clear explanation of your decision",
  "feedback": "Specific feedback for the implementor if rejected, or null if approved"
}

Be concise but thorough in your reasoning. If rejecting, provide actionable feedback."#.to_string()
}

/// Build the user prompt with PR and issue context.
pub fn build_evaluation_prompt(
    pr: &PullRequest,
    issue: Option<&Issue>,
    task_title: &str,
    task_description: Option<&str>,
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

    // PR context
    prompt.push_str("## Pull Request\n\n");
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

    // PR body
    if let Some(body) = &pr.body {
        if !body.is_empty() {
            prompt.push_str("### PR Description\n\n");
            prompt.push_str(&format!("{}\n\n", truncate_text(body, 2000)));
        }
    }

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

    // PR comments
    if !pr.comments.is_empty() {
        prompt.push_str("### PR Comments\n\n");
        for comment in pr.comments.iter().take(5) {
            prompt.push_str(&format!(
                "**@{}**: {}\n\n",
                comment.author.login,
                truncate_text(&comment.body, 500)
            ));
        }
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

    prompt.push_str("## Evaluation Request\n\n");
    prompt.push_str("Based on the above context, evaluate whether this PR is ready to merge. ");
    prompt.push_str("Consider issue alignment, test status, merge conflicts, and code quality. ");
    prompt.push_str("Respond with your evaluation in the JSON format specified.");

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
        assert!(prompt.contains("Issue Alignment"));
        assert!(prompt.contains("Test/CI Status"));
        assert!(prompt.contains("Merge Conflicts"));
        assert!(prompt.contains("Project Conventions"));
        assert!(prompt.contains("JSON"));
    }

    #[test]
    fn test_build_evaluation_prompt_basic() {
        let pr = test_pr();
        let prompt = build_evaluation_prompt(&pr, None, "Fix auth bug", None);

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

        let prompt = build_evaluation_prompt(&pr, None, "Test", None);
        assert!(prompt.contains("has conflicts"));
    }

    #[test]
    fn test_build_evaluation_prompt_changes_requested() {
        let mut pr = test_pr();
        pr.review_decision = Some(ReviewDecision::ChangesRequested);

        let prompt = build_evaluation_prompt(&pr, None, "Test", None);
        assert!(prompt.contains("Changes requested"));
    }

    #[test]
    fn test_build_evaluation_prompt_with_reviews() {
        let pr = test_pr();
        let prompt = build_evaluation_prompt(&pr, None, "Test", None);

        // Should contain review info
        assert!(prompt.contains("@testuser"));
        assert!(prompt.contains("LGTM"));
    }
}
