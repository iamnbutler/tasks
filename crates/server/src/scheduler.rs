//! Scheduler — spec §3.2, §12.1.
//!
//! Discovers work from GitHub and creates tasks.

use std::collections::HashMap;

/// Canonical label that always causes an issue/PR to be skipped, regardless of
/// project configuration. This label is checked in addition to the configurable
/// `labels.ignore` list from `workflow.toml`.
pub const SKIP_LABEL: &str = "tasks/skip";

use thiserror::Error;

use tasks_github::model::{Issue, PullRequest};
use crate::model::task::{Task, TaskSource, TaskState};
use crate::workflow::LabelConfig;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("github error: {0}")]
    GitHub(#[from] tasks_github::GitHubError),
}

/// Convert a GitHub issue into a Task, if it should be imported.
/// Returns None if the issue should be ignored (matches ignore labels or canonical skip label).
pub fn issue_to_task(
    issue: &Issue,
    project_id: &str,
    label_config: &LabelConfig,
) -> Option<Task> {
    // Skip closed issues.
    if issue.state != tasks_github::model::IssueState::Open {
        return None;
    }

    let issue_label_names: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();

    // Check for the canonical skip label — always skip regardless of config.
    if issue_label_names.contains(&SKIP_LABEL) {
        return None;
    }

    // Check if any issue label matches an ignore label — if so, skip.
    for ignore in &label_config.ignore {
        if issue_label_names.contains(&ignore.as_str()) {
            return None;
        }
    }

    let id = format!("gh-{}-{}-issue-{}", issue.owner, issue.repo, issue.number);
    let source = TaskSource::GithubIssue {
        owner: issue.owner.clone(),
        repo: issue.repo.clone(),
        number: issue.number,
    };

    let mut task = Task::new(id, source, issue.title.clone(), project_id);
    task.description = issue.body.clone();
    task.labels = issue.labels.iter().map(|l| l.name.clone()).collect();
    task.source_created_at = Some(issue.created_at);

    // If any label matches a blocked label, set state to Blocked.
    let is_blocked = label_config.blocked.iter().any(|b| issue_label_names.contains(&b.as_str()));
    if is_blocked {
        task.state = TaskState::Blocked;
    }

    Some(task)
}

/// Convert a GitHub PR into a Task, if it should be imported.
/// Returns None if the PR should be ignored (matches ignore labels or canonical skip label).
pub fn pr_to_task(
    pr: &PullRequest,
    project_id: &str,
    label_config: &LabelConfig,
) -> Option<Task> {
    // Skip closed/merged PRs.
    if pr.state != tasks_github::model::PullRequestState::Open {
        return None;
    }

    let pr_label_names: Vec<&str> = pr.labels.iter().map(|l| l.name.as_str()).collect();

    // Check for the canonical skip label — always skip regardless of config.
    if pr_label_names.contains(&SKIP_LABEL) {
        return None;
    }

    // Check if any PR label matches an ignore label — if so, skip.
    for ignore in &label_config.ignore {
        if pr_label_names.contains(&ignore.as_str()) {
            return None;
        }
    }

    let id = format!("gh-{}-{}-pr-{}", pr.owner, pr.repo, pr.number);
    let source = TaskSource::GithubPr {
        owner: pr.owner.clone(),
        repo: pr.repo.clone(),
        number: pr.number,
    };

    let mut task = Task::new(id, source, pr.title.clone(), project_id);
    task.description = pr.body.clone();
    task.labels = pr.labels.iter().map(|l| l.name.clone()).collect();
    task.source_created_at = Some(pr.created_at);

    // If any label matches a blocked label, set state to Blocked.
    let is_blocked = label_config.blocked.iter().any(|b| pr_label_names.contains(&b.as_str()));
    if is_blocked {
        task.state = TaskState::Blocked;
    }

    Some(task)
}

/// Check if a task already exists for a given GitHub source.
/// Used to deduplicate — don't create tasks for issues we've already imported.
pub fn task_exists_for_source(
    tasks: &HashMap<String, Task>,
    source: &TaskSource,
) -> bool {
    tasks.values().any(|t| match (&t.source, source) {
        (
            TaskSource::GithubIssue { owner: o1, repo: r1, number: n1 },
            TaskSource::GithubIssue { owner: o2, repo: r2, number: n2 },
        ) => o1 == o2 && r1 == r2 && n1 == n2,
        (
            TaskSource::GithubPr { owner: o1, repo: r1, number: n1 },
            TaskSource::GithubPr { owner: o2, repo: r2, number: n2 },
        ) => o1 == o2 && r1 == r2 && n1 == n2,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tasks_github::model::{IssueState as GhIssueState, PullRequestState, Label, User};

    fn make_label(name: &str) -> Label {
        Label {
            name: name.to_string(),
            color: "000000".to_string(),
        }
    }

    fn make_user() -> User {
        User {
            login: "testuser".to_string(),
            node_id: "U_1".to_string(),
        }
    }

    fn make_issue(number: u64, labels: Vec<Label>, state: GhIssueState) -> Issue {
        let now = Utc::now();
        Issue {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number,
            node_id: format!("I_{number}"),
            title: format!("Issue #{number}"),
            body: Some(format!("Body of issue #{number}")),
            state,
            state_reason: None,
            labels,
            assignees: vec![],
            milestone: None,
            comments: vec![],
            sub_issues: vec![],
            linked_pull_requests: vec![],
            author: make_user(),
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    }

    fn make_pr(number: u64, labels: Vec<Label>, state: PullRequestState) -> PullRequest {
        let now = Utc::now();
        PullRequest {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number,
            node_id: format!("PR_{number}"),
            title: format!("PR #{number}"),
            body: Some(format!("Body of PR #{number}")),
            state,
            head_ref: "feature".to_string(),
            head_sha: "abc123".to_string(),
            base_ref: "main".to_string(),
            is_draft: false,
            mergeable: None,
            labels,
            assignees: vec![],
            review_decision: None,
            reviews: vec![],
            comments: vec![],
            linked_issues: vec![],
            author: make_user(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            merged_at: None,
        }
    }

    fn default_label_config() -> LabelConfig {
        LabelConfig {
            ignore: vec!["wontfix".to_string()],
            blocked: vec!["needs-design".to_string()],
        }
    }

    #[test]
    fn issue_to_task_basic() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg).expect("should produce a task");

        assert_eq!(task.id, "gh-acme-widgets-issue-42");
        assert_eq!(task.title, "Issue #42");
        assert_eq!(task.description.as_deref(), Some("Body of issue #42"));
        assert_eq!(task.project, "proj-1");
        assert_eq!(task.labels, vec!["bug"]);
        assert_eq!(task.state, TaskState::Waiting);
        matches!(task.source, TaskSource::GithubIssue { owner, repo, number } if owner == "acme" && repo == "widgets" && number == 42);
    }

    #[test]
    fn issue_to_task_ignored() {
        let issue = make_issue(7, vec![make_label("wontfix")], GhIssueState::Open);
        let cfg = default_label_config();

        let result = issue_to_task(&issue, "proj-1", &cfg);
        assert!(result.is_none(), "issue with ignore label should be skipped");
    }

    #[test]
    fn issue_to_task_blocked() {
        let issue = make_issue(
            10,
            vec![make_label("enhancement"), make_label("needs-design")],
            GhIssueState::Open,
        );
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg).expect("should produce a task");

        assert_eq!(task.state, TaskState::Blocked);
        assert_eq!(task.labels, vec!["enhancement", "needs-design"]);
    }

    #[test]
    fn pr_to_task_basic() {
        let pr = make_pr(99, vec![make_label("feature")], PullRequestState::Open);
        let cfg = default_label_config();

        let task = pr_to_task(&pr, "proj-2", &cfg).expect("should produce a task");

        assert_eq!(task.id, "gh-acme-widgets-pr-99");
        assert_eq!(task.title, "PR #99");
        assert_eq!(task.description.as_deref(), Some("Body of PR #99"));
        assert_eq!(task.project, "proj-2");
        assert_eq!(task.labels, vec!["feature"]);
        assert_eq!(task.state, TaskState::Waiting);
        matches!(task.source, TaskSource::GithubPr { owner, repo, number } if owner == "acme" && repo == "widgets" && number == 99);
    }

    #[test]
    fn pr_to_task_ignored() {
        let pr = make_pr(5, vec![make_label("wontfix")], PullRequestState::Open);
        let cfg = default_label_config();

        let result = pr_to_task(&pr, "proj-2", &cfg);
        assert!(result.is_none(), "PR with ignore label should be skipped");
    }

    #[test]
    fn task_exists_dedup() {
        let issue = make_issue(42, vec![], GhIssueState::Open);
        let cfg = LabelConfig::default();
        let task = issue_to_task(&issue, "proj-1", &cfg).unwrap();

        let mut tasks = HashMap::new();
        tasks.insert(task.id.clone(), task);

        let source = TaskSource::GithubIssue {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number: 42,
        };

        assert!(task_exists_for_source(&tasks, &source));
    }

    #[test]
    fn task_not_exists() {
        let tasks: HashMap<String, Task> = HashMap::new();

        let source = TaskSource::GithubIssue {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number: 999,
        };

        assert!(!task_exists_for_source(&tasks, &source));
    }

    #[test]
    fn closed_issue_not_imported() {
        let issue = make_issue(55, vec![make_label("bug")], GhIssueState::Closed);
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg);
        assert!(task.is_none(), "closed issues should be skipped");
    }

    #[test]
    fn issue_with_canonical_skip_label() {
        // The canonical tasks/skip label should always skip, regardless of config.
        let issue = make_issue(
            100,
            vec![make_label("bug"), make_label(super::SKIP_LABEL)],
            GhIssueState::Open,
        );
        // Use an empty ignore list to prove the canonical label works independently.
        let cfg = LabelConfig {
            ignore: vec![],
            blocked: vec![],
        };

        let result = issue_to_task(&issue, "proj-1", &cfg);
        assert!(result.is_none(), "issue with tasks/skip label should be skipped");
    }

    #[test]
    fn pr_with_canonical_skip_label() {
        // The canonical tasks/skip label should always skip PRs too.
        let pr = make_pr(
            101,
            vec![make_label("feature"), make_label(super::SKIP_LABEL)],
            PullRequestState::Open,
        );
        // Use an empty ignore list to prove the canonical label works independently.
        let cfg = LabelConfig {
            ignore: vec![],
            blocked: vec![],
        };

        let result = pr_to_task(&pr, "proj-2", &cfg);
        assert!(result.is_none(), "PR with tasks/skip label should be skipped");
    }

    #[test]
    fn canonical_skip_label_constant() {
        // Verify the canonical label value.
        assert_eq!(super::SKIP_LABEL, "tasks/skip");
    }
}
