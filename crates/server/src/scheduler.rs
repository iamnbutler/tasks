//! Scheduler — spec §3.2, §11.3, §12.1.
//!
//! Discovers work from GitHub and creates tasks.
//! Detects external closures of tracked issues/PRs (spec §11.3).

use std::collections::HashMap;

use thiserror::Error;

use tasks_github::model::{Issue, IssueState as GhIssueState, PullRequest, PullRequestState};
use crate::model::task::{Task, TaskSource, TaskState};
use crate::workflow::LabelConfig;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("github error: {0}")]
    GitHub(#[from] tasks_github::GitHubError),
}

/// Represents an externally closed issue or PR that requires a task state transition.
///
/// Spec §11.3: "When a GitHub issue is closed externally, the corresponding task
/// should be cancelled or completed (depending on context)."
#[derive(Debug, Clone)]
pub struct ExternalClosure {
    /// The task ID to transition.
    pub task_id: String,
    /// The new state for the task.
    pub new_state: TaskState,
    /// Human-readable reason for the closure.
    pub reason: String,
}

/// Extract tasks that need external closure checking.
///
/// Returns tasks that:
/// - Are sourced from a GitHub issue or PR
/// - Are not in a terminal state (Completed, Failed, Cancelled)
///
/// The caller should then fetch the current state of each task's source
/// from GitHub and check if it has been closed.
pub fn tasks_needing_closure_check(tasks: &HashMap<String, Task>) -> Vec<&Task> {
    tasks
        .values()
        .filter(|t| {
            // Only check GitHub-sourced tasks
            !matches!(t.source, TaskSource::Internal)
            // Skip tasks already in terminal state
            && !t.state.is_terminal()
        })
        .collect()
}

/// Determine if an issue closure should trigger a task state change.
///
/// Returns the new task state if the issue is closed, None if open.
/// - Closed issues → Cancelled (spec §11.3)
pub fn check_issue_closure(issue: &Issue) -> Option<(TaskState, String)> {
    if issue.state == GhIssueState::Closed {
        let reason = match issue.state_reason {
            Some(tasks_github::model::IssueStateReason::Completed) => {
                "issue closed as completed".to_string()
            }
            Some(tasks_github::model::IssueStateReason::NotPlanned) => {
                "issue closed as not planned".to_string()
            }
            _ => "issue closed externally".to_string(),
        };
        Some((TaskState::Cancelled, reason))
    } else {
        None
    }
}

/// Determine if a PR closure should trigger a task state change.
///
/// Returns the new task state if the PR is closed/merged, None if open.
/// - Merged PRs → Completed (spec §11.3)
/// - Closed (not merged) PRs → Cancelled (spec §11.3)
pub fn check_pr_closure(pr: &PullRequest) -> Option<(TaskState, String)> {
    match pr.state {
        PullRequestState::Merged => {
            Some((TaskState::Completed, "PR merged externally".to_string()))
        }
        PullRequestState::Closed => {
            Some((TaskState::Cancelled, "PR closed without merge".to_string()))
        }
        PullRequestState::Open => None,
    }
}

/// Convert a GitHub issue into a Task, if it should be imported.
/// Returns None if the issue should be ignored (matches ignore labels).
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
            parent: None,
            sub_issues: vec![],
            blocked_by: vec![],
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

    // --- External closure detection tests (spec §11.3) ---

    fn make_issue_with_state_reason(
        number: u64,
        state: GhIssueState,
        state_reason: Option<tasks_github::model::IssueStateReason>,
    ) -> Issue {
        let mut issue = make_issue(number, vec![], state);
        issue.state_reason = state_reason;
        issue
    }

    #[test]
    fn tasks_needing_closure_check_filters_terminal_states() {
        let cfg = LabelConfig::default();
        let issue = make_issue(1, vec![], GhIssueState::Open);
        let mut task = issue_to_task(&issue, "proj-1", &cfg).unwrap();

        let mut tasks = HashMap::new();

        // Waiting task should be checked
        tasks.insert(task.id.clone(), task.clone());
        assert_eq!(tasks_needing_closure_check(&tasks).len(), 1);

        // Running task should be checked
        task.state = TaskState::Running;
        tasks.insert(task.id.clone(), task.clone());
        assert_eq!(tasks_needing_closure_check(&tasks).len(), 1);

        // Completed task should NOT be checked (terminal)
        task.state = TaskState::Completed;
        tasks.insert(task.id.clone(), task.clone());
        assert_eq!(tasks_needing_closure_check(&tasks).len(), 0);

        // Failed task should NOT be checked (terminal)
        task.state = TaskState::Failed;
        tasks.insert(task.id.clone(), task.clone());
        assert_eq!(tasks_needing_closure_check(&tasks).len(), 0);

        // Cancelled task should NOT be checked (terminal)
        task.state = TaskState::Cancelled;
        tasks.insert(task.id.clone(), task.clone());
        assert_eq!(tasks_needing_closure_check(&tasks).len(), 0);
    }

    #[test]
    fn tasks_needing_closure_check_filters_internal_tasks() {
        let mut task = Task::new(
            "internal-task-1",
            TaskSource::Internal,
            "Internal work",
            "proj-1",
        );
        task.state = TaskState::Running;

        let mut tasks = HashMap::new();
        tasks.insert(task.id.clone(), task);

        // Internal tasks should NOT be checked
        assert_eq!(tasks_needing_closure_check(&tasks).len(), 0);
    }

    #[test]
    fn check_issue_closure_open_issue() {
        let issue = make_issue(42, vec![], GhIssueState::Open);
        assert!(check_issue_closure(&issue).is_none());
    }

    #[test]
    fn check_issue_closure_closed_issue() {
        let issue = make_issue(42, vec![], GhIssueState::Closed);
        let result = check_issue_closure(&issue);
        assert!(result.is_some());
        let (state, reason) = result.unwrap();
        assert_eq!(state, TaskState::Cancelled);
        assert!(reason.contains("closed"));
    }

    #[test]
    fn check_issue_closure_closed_as_completed() {
        let issue = make_issue_with_state_reason(
            42,
            GhIssueState::Closed,
            Some(tasks_github::model::IssueStateReason::Completed),
        );
        let result = check_issue_closure(&issue);
        assert!(result.is_some());
        let (state, reason) = result.unwrap();
        assert_eq!(state, TaskState::Cancelled);
        assert!(reason.contains("completed"));
    }

    #[test]
    fn check_issue_closure_closed_as_not_planned() {
        let issue = make_issue_with_state_reason(
            42,
            GhIssueState::Closed,
            Some(tasks_github::model::IssueStateReason::NotPlanned),
        );
        let result = check_issue_closure(&issue);
        assert!(result.is_some());
        let (state, reason) = result.unwrap();
        assert_eq!(state, TaskState::Cancelled);
        assert!(reason.contains("not planned"));
    }

    #[test]
    fn check_pr_closure_open_pr() {
        let pr = make_pr(99, vec![], PullRequestState::Open);
        assert!(check_pr_closure(&pr).is_none());
    }

    #[test]
    fn check_pr_closure_merged_pr() {
        let pr = make_pr(99, vec![], PullRequestState::Merged);
        let result = check_pr_closure(&pr);
        assert!(result.is_some());
        let (state, reason) = result.unwrap();
        assert_eq!(state, TaskState::Completed);
        assert!(reason.contains("merged"));
    }

    #[test]
    fn check_pr_closure_closed_without_merge() {
        let pr = make_pr(99, vec![], PullRequestState::Closed);
        let result = check_pr_closure(&pr);
        assert!(result.is_some());
        let (state, reason) = result.unwrap();
        assert_eq!(state, TaskState::Cancelled);
        assert!(reason.contains("closed without merge"));
    }
}
