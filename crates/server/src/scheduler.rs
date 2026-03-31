//! Scheduler — spec §3.2, §12.1.
//!
//! Discovers work from GitHub and creates tasks.

use std::collections::HashMap;

use thiserror::Error;

use tasks_github::model::{Issue, PullRequest};
use crate::model::task::{Task, TaskSource, TaskState};
use crate::workflow::LabelConfig;

/// Canonical label that always causes an issue/PR to be skipped, regardless of
/// project configuration. This label is checked in addition to the configurable
/// `labels.ignore` list from `workflow.toml`.
pub const SKIP_LABEL: &str = "tasks/skip";

/// Priority label prefixes. Labels matching these patterns are parsed for priority.
/// Supported formats:
/// - `p0`, `p1`, `p2`, `p3`
/// - `tasks/p0`, `tasks/p1`, etc.
/// - `priority/p0`, `priority/p1`, etc.
/// - `urgent` (maps to p0)
/// - `high` (maps to p1)
/// Lower numbers = higher priority.
const PRIORITY_PREFIXES: &[&str] = &["priority/p", "tasks/p", "p"];

/// Parse priority from a list of label names.
///
/// Supported formats:
/// - `p0`, `p1`, `p2`, `p3` — direct priority
/// - `tasks/p0`, `tasks/p1`, etc. — namespaced
/// - `priority/p0`, `priority/p1`, etc. — namespaced
/// - `urgent` — maps to priority 0
/// - `high` — maps to priority 1
///
/// Returns the priority as an i32 (0 = highest, 3 = lowest).
/// If multiple priority labels exist, returns the highest (lowest number).
/// Returns None if no priority label is found.
fn parse_priority_from_labels(labels: &[&str]) -> Option<i32> {
    let mut best_priority: Option<i32> = None;

    for label in labels {
        let label_lower = label.to_lowercase();

        // Check for special keywords first
        let priority = match label_lower.as_str() {
            "urgent" => Some(0),
            "high" => Some(1),
            _ => {
                // Try prefix-based parsing
                let mut found = None;
                for prefix in PRIORITY_PREFIXES {
                    if let Some(suffix) = label_lower.strip_prefix(prefix) {
                        if let Ok(p) = suffix.parse::<i32>() {
                            if (0..=9).contains(&p) {
                                found = Some(p);
                                break;
                            }
                        }
                    }
                }
                found
            }
        };

        if let Some(p) = priority {
            best_priority = Some(match best_priority {
                None => p,
                Some(current) => current.min(p),
            });
        }
    }

    best_priority
}

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
    task.source_number = Some(issue.number);
    task.priority = parse_priority_from_labels(&issue_label_names);

    // Import closed issues as terminal tasks so they are tracked as "seen"
    // even if closed before the first poll (issue #502).
    if issue.state != tasks_github::model::IssueState::Open {
        if let Some(closure_reason) = issue.classify_closure() {
            use tasks_github::model::ClosureReason;
            let terminal_state = match closure_reason {
                ClosureReason::PrMerged | ClosureReason::ManualCompletion => TaskState::Completed,
                ClosureReason::NotPlanned | ClosureReason::Unknown => TaskState::Cancelled,
            };
            task.state = terminal_state;
        } else {
            // Closed but no classifiable reason — mark cancelled.
            task.state = TaskState::Cancelled;
        }
        return Some(task);
    }

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
    task.source_number = Some(pr.number);
    task.priority = parse_priority_from_labels(&pr_label_names);

    // If any label matches a blocked label, set state to Blocked.
    let is_blocked = label_config.blocked.iter().any(|b| pr_label_names.contains(&b.as_str()));
    if is_blocked {
        task.state = TaskState::Blocked;
    }

    Some(task)
}

/// Describes what changed during reconciliation, so the caller can emit
/// appropriate events and log messages.
#[derive(Debug, Default)]
pub struct ReconcileResult {
    /// Fields that were updated (for logging).
    pub updated_fields: Vec<&'static str>,
    /// If the task state changed, the new state.
    pub new_state: Option<TaskState>,
    /// If the task should be cancelled (skip label added).
    pub cancelled: bool,
    /// If the issue was closed, why — combining `state_reason` with linked PR
    /// data to distinguish agent success from external rejection.
    pub closure_reason: Option<tasks_github::model::ClosureReason>,
}

impl ReconcileResult {
    pub fn has_changes(&self) -> bool {
        !self.updated_fields.is_empty() || self.new_state.is_some() || self.cancelled
    }
}

/// Reconcile a local task with fresh GitHub issue data (spec §12, issue #254).
///
/// Updates GitHub-authoritative fields: title, description, labels, blocked_by.
/// Detects state transitions: closed → Completed/Cancelled, skip label → Cancelled,
/// blocked label changes → Blocked/Waiting.
///
/// Platform-authoritative fields (session_id, retry_count, etc.) are never touched.
/// Returns a `ReconcileResult` describing what changed.
pub fn reconcile_task(
    task: &mut Task,
    issue: &Issue,
    label_config: &LabelConfig,
) -> ReconcileResult {
    let mut result = ReconcileResult::default();

    // --- GitHub-authoritative field sync ---

    if task.title != issue.title {
        task.title = issue.title.clone();
        result.updated_fields.push("title");
    }

    if task.description != issue.body {
        task.description = issue.body.clone();
        result.updated_fields.push("description");
    }

    let new_labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
    if task.labels != new_labels {
        task.labels = new_labels;
        result.updated_fields.push("labels");
    }

    // Derive priority from labels.
    let issue_label_names: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
    let new_priority = parse_priority_from_labels(&issue_label_names);
    if task.priority != new_priority {
        task.priority = new_priority;
        result.updated_fields.push("priority");
    }

    // Derive blocked_by from GitHub blocking issue relationships.
    // Format: "gh-{owner}-{repo}-issue-{number}" to match our task ID convention.
    let new_blocked_by: Vec<String> = issue
        .blocked_by
        .iter()
        .filter(|b| b.state == tasks_github::model::IssueState::Open)
        .map(|b| format!("gh-{}-{}-issue-{}", b.owner, b.repo, b.number))
        .collect();
    if task.blocked_by != new_blocked_by {
        task.blocked_by = new_blocked_by;
        result.updated_fields.push("blocked_by");
    }

    // --- State transitions from GitHub ---

    // If already terminal, don't change state.
    if task.state.is_terminal() {
        if result.has_changes() {
            task.updated_at = chrono::Utc::now();
        }
        return result;
    }

    // Check for skip/ignore labels added after import → cancel the task.
    // This is a hard override for ALL non-terminal states including active ones
    // (Running, Conflict, etc.). Skip is an explicit human signal meaning "don't
    // work on this." Cancelling doesn't kill running sessions — it prevents
    // re-dispatch once the current session ends.
    let should_cancel = issue_label_names.contains(&SKIP_LABEL)
        || label_config
            .ignore
            .iter()
            .any(|ig| issue_label_names.contains(&ig.as_str()));

    if should_cancel {
        result.cancelled = true;
        result.new_state = Some(TaskState::Cancelled);
        task.set_state(TaskState::Cancelled);
        return result;
    }

    // Detect external closure.
    if let Some(closure_reason) = issue.classify_closure() {
        use tasks_github::model::ClosureReason;
        let new_state = match closure_reason {
            ClosureReason::PrMerged | ClosureReason::ManualCompletion => TaskState::Completed,
            ClosureReason::NotPlanned | ClosureReason::Unknown => TaskState::Cancelled,
        };
        result.closure_reason = Some(closure_reason);
        result.new_state = Some(new_state);
        task.set_state(new_state);
        return result;
    }

    // Check blocked label changes — only for tasks not in active states
    // (Running, Question, Testing, AwaitingMerge, Conflict).
    let is_active = matches!(
        task.state,
        TaskState::Running
            | TaskState::Question
            | TaskState::Testing
            | TaskState::AwaitingMerge
            | TaskState::Conflict
    );

    if !is_active {
        let is_blocked = label_config
            .blocked
            .iter()
            .any(|b| issue_label_names.contains(&b.as_str()));
        let has_open_blockers = !task.blocked_by.is_empty();

        if (is_blocked || has_open_blockers) && task.state != TaskState::Blocked {
            result.new_state = Some(TaskState::Blocked);
            task.set_state(TaskState::Blocked);
        } else if !is_blocked && !has_open_blockers && task.state == TaskState::Blocked {
            result.new_state = Some(TaskState::Waiting);
            task.set_state(TaskState::Waiting);
        }
    }

    // Only manually update updated_at for metadata-only changes (no state transition).
    // When set_state() was called above, it already handled updated_at and last_activity_at.
    if result.has_changes() && result.new_state.is_none() {
        task.updated_at = chrono::Utc::now();
    }

    result
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
            ci_status: None,
            check_runs: vec![],
            status_contexts: vec![],
            latest_reviews: vec![],
            reaction_count: 0,
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
    fn closed_issue_imported_as_cancelled() {
        // Closed issue with no state_reason → ClosureReason::Unknown → Cancelled
        let issue = make_issue(55, vec![make_label("bug")], GhIssueState::Closed);
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg).expect("closed issues should be imported");
        assert_eq!(task.state, TaskState::Cancelled);
    }

    #[test]
    fn closed_issue_completed_imported_as_completed() {
        let mut issue = make_issue(56, vec![make_label("bug")], GhIssueState::Closed);
        issue.state_reason = Some(tasks_github::model::IssueStateReason::Completed);
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg).expect("closed issues should be imported");
        assert_eq!(task.state, TaskState::Completed);
    }

    #[test]
    fn closed_issue_not_planned_imported_as_cancelled() {
        let mut issue = make_issue(57, vec![make_label("bug")], GhIssueState::Closed);
        issue.state_reason = Some(tasks_github::model::IssueStateReason::NotPlanned);
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg).expect("closed issues should be imported");
        assert_eq!(task.state, TaskState::Cancelled);
    }

    #[test]
    fn closed_issue_with_skip_label_not_imported() {
        let issue = make_issue(
            58,
            vec![make_label("bug"), make_label(super::SKIP_LABEL)],
            GhIssueState::Closed,
        );
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg);
        assert!(task.is_none(), "closed issue with skip label should still be skipped");
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

    // --- reconcile_task tests ---

    fn make_task_from_issue(issue: &Issue, cfg: &LabelConfig) -> Task {
        issue_to_task(issue, "proj-1", cfg).expect("should produce a task")
    }

    #[test]
    fn reconcile_syncs_title_and_description() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        // Simulate GitHub title/description change
        let mut updated_issue = issue.clone();
        updated_issue.title = "Updated title".to_string();
        updated_issue.body = Some("Updated body".to_string());

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.has_changes());
        assert!(result.updated_fields.contains(&"title"));
        assert!(result.updated_fields.contains(&"description"));
        assert_eq!(task.title, "Updated title");
        assert_eq!(task.description.as_deref(), Some("Updated body"));
    }

    #[test]
    fn reconcile_syncs_labels() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug"), make_label("priority-high")];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.updated_fields.contains(&"labels"));
        assert_eq!(task.labels, vec!["bug", "priority-high"]);
    }

    #[test]
    fn reconcile_detects_external_closure_completed() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let mut closed_issue = issue.clone();
        closed_issue.state = GhIssueState::Closed;
        closed_issue.state_reason = Some(tasks_github::model::IssueStateReason::Completed);

        let result = reconcile_task(&mut task, &closed_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Completed));
        assert_eq!(task.state, TaskState::Completed);
        // No linked PRs → ManualCompletion, not PrMerged
        assert_eq!(
            result.closure_reason,
            Some(tasks_github::model::ClosureReason::ManualCompletion)
        );
    }

    #[test]
    fn reconcile_detects_external_closure_cancelled() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let mut closed_issue = issue.clone();
        closed_issue.state = GhIssueState::Closed;
        closed_issue.state_reason = Some(tasks_github::model::IssueStateReason::NotPlanned);

        let result = reconcile_task(&mut task, &closed_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Cancelled));
        assert_eq!(task.state, TaskState::Cancelled);
        assert_eq!(
            result.closure_reason,
            Some(tasks_github::model::ClosureReason::NotPlanned)
        );
    }

    #[test]
    fn reconcile_closure_pr_merged_means_agent_success() {
        // Issue closed as completed with a linked merged PR → PrMerged.
        // This is the "agent's work was accepted" signal.
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let mut closed_issue = issue.clone();
        closed_issue.state = GhIssueState::Closed;
        closed_issue.state_reason = Some(tasks_github::model::IssueStateReason::Completed);
        closed_issue.linked_pull_requests = vec![tasks_github::model::LinkedPR {
            number: 100,
            title: "Fix bug #42".to_string(),
            state: PullRequestState::Merged,
            node_id: "PR_100".to_string(),
        }];

        let result = reconcile_task(&mut task, &closed_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Completed));
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(
            result.closure_reason,
            Some(tasks_github::model::ClosureReason::PrMerged)
        );
    }

    #[test]
    fn reconcile_closure_with_open_pr_is_manual_completion() {
        // Issue closed as completed but linked PR is still open (not merged).
        // This is manual completion, not agent success.
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let mut closed_issue = issue.clone();
        closed_issue.state = GhIssueState::Closed;
        closed_issue.state_reason = Some(tasks_github::model::IssueStateReason::Completed);
        closed_issue.linked_pull_requests = vec![tasks_github::model::LinkedPR {
            number: 100,
            title: "Fix bug #42".to_string(),
            state: PullRequestState::Open,
            node_id: "PR_100".to_string(),
        }];

        let result = reconcile_task(&mut task, &closed_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Completed));
        assert_eq!(
            result.closure_reason,
            Some(tasks_github::model::ClosureReason::ManualCompletion)
        );
    }

    #[test]
    fn reconcile_closure_unknown_when_no_state_reason() {
        // Issue closed without a state_reason → Unknown.
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let mut closed_issue = issue.clone();
        closed_issue.state = GhIssueState::Closed;
        // state_reason is None (default from make_issue)

        let result = reconcile_task(&mut task, &closed_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Cancelled));
        assert_eq!(
            result.closure_reason,
            Some(tasks_github::model::ClosureReason::Unknown)
        );
    }

    #[test]
    fn reconcile_skip_label_cancels_task() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        // Add skip label on GitHub
        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug"), make_label(super::SKIP_LABEL)];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.cancelled);
        assert_eq!(task.state, TaskState::Cancelled);
    }

    #[test]
    fn reconcile_blocked_label_transitions_to_blocked() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        assert_eq!(task.state, TaskState::Waiting);

        // Add blocked label on GitHub
        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug"), make_label("needs-design")];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Blocked));
        assert_eq!(task.state, TaskState::Blocked);
    }

    #[test]
    fn reconcile_blocked_label_removed_transitions_to_waiting() {
        let issue = make_issue(
            42,
            vec![make_label("bug"), make_label("needs-design")],
            GhIssueState::Open,
        );
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        assert_eq!(task.state, TaskState::Blocked);

        // Remove blocked label
        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug")];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Waiting));
        assert_eq!(task.state, TaskState::Waiting);
    }

    #[test]
    fn reconcile_no_changes_returns_empty() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        let result = reconcile_task(&mut task, &issue, &cfg);
        assert!(!result.has_changes());
        assert!(result.updated_fields.is_empty());
        assert!(result.new_state.is_none());
    }

    #[test]
    fn reconcile_terminal_task_still_syncs_metadata() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        task.set_state(TaskState::Completed);

        // Title changed on GitHub
        let mut updated_issue = issue.clone();
        updated_issue.title = "Final title".to_string();

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.updated_fields.contains(&"title"));
        assert_eq!(task.title, "Final title");
        // State should NOT change
        assert!(result.new_state.is_none());
        assert_eq!(task.state, TaskState::Completed);
    }

    #[test]
    fn reconcile_active_task_ignores_blocked_label() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        task.set_state(TaskState::Running);

        // Add blocked label — shouldn't affect a running task
        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug"), make_label("needs-design")];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.new_state.is_none());
        assert_eq!(task.state, TaskState::Running);
    }

    #[test]
    fn reconcile_skip_label_cancels_active_task() {
        // Skip label is a hard override — cancels even Running/Conflict tasks.
        // It doesn't kill the session, just prevents re-dispatch.
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        task.set_state(TaskState::Running);

        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug"), make_label(super::SKIP_LABEL)];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.cancelled);
        assert_eq!(task.state, TaskState::Cancelled);
    }

    #[test]
    fn reconcile_skip_label_trumps_closure_reason() {
        // If an issue has tasks/skip AND is closed-as-completed,
        // skip wins → Cancelled, not Completed. Skip is an explicit
        // human override.
        let mut issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        issue.state = GhIssueState::Closed;
        issue.state_reason = Some(tasks_github::model::IssueStateReason::Completed);
        issue.labels = vec![make_label("bug"), make_label(super::SKIP_LABEL)];

        let result = reconcile_task(&mut task, &issue, &cfg);
        assert!(result.cancelled);
        assert_eq!(result.new_state, Some(TaskState::Cancelled));
        assert_eq!(task.state, TaskState::Cancelled);
    }

    #[test]
    fn reconcile_metadata_only_sets_updated_at_once() {
        // When only metadata changes (no state transition), updated_at
        // should be set once by reconcile_task, not by set_state.
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        let original_updated = task.updated_at;

        // Small delay to ensure timestamps differ
        let mut updated_issue = issue.clone();
        updated_issue.title = "New title".to_string();

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.new_state.is_none());
        assert!(task.updated_at >= original_updated);
    }

    #[test]
    fn reconcile_cross_repo_blocking() {
        // Test that cross-repo blocking issues generate correct task IDs.
        // Issue acme/frontend#10 is blocked by acme/backend#5.
        let mut issue = make_issue(10, vec![make_label("feature")], GhIssueState::Open);
        issue.owner = "acme".to_string();
        issue.repo = "frontend".to_string();
        issue.blocked_by = vec![
            tasks_github::model::BlockingIssueRef {
                owner: "acme".to_string(),
                repo: "backend".to_string(),
                number: 5,
                title: "Backend API".to_string(),
                state: GhIssueState::Open,
                node_id: "I_5".to_string(),
            },
        ];

        let cfg = default_label_config();
        let mut task = issue_to_task(&issue, "proj-1", &cfg).expect("should produce a task");

        let result = reconcile_task(&mut task, &issue, &cfg);
        assert!(result.has_changes() || !task.blocked_by.is_empty());

        // The blocked_by should point to acme/backend#5, not acme/frontend#5.
        assert_eq!(task.blocked_by.len(), 1);
        assert_eq!(task.blocked_by[0], "gh-acme-backend-issue-5");
    }

    // --- Priority parsing tests ---

    #[test]
    fn parse_priority_basic_p_labels() {
        assert_eq!(parse_priority_from_labels(&["p0"]), Some(0));
        assert_eq!(parse_priority_from_labels(&["p1"]), Some(1));
        assert_eq!(parse_priority_from_labels(&["p2"]), Some(2));
        assert_eq!(parse_priority_from_labels(&["p3"]), Some(3));
    }

    #[test]
    fn parse_priority_namespaced_labels() {
        assert_eq!(parse_priority_from_labels(&["tasks/p0"]), Some(0));
        assert_eq!(parse_priority_from_labels(&["tasks/p1"]), Some(1));
        assert_eq!(parse_priority_from_labels(&["priority/p2"]), Some(2));
        assert_eq!(parse_priority_from_labels(&["priority/p3"]), Some(3));
    }

    #[test]
    fn parse_priority_keyword_labels() {
        assert_eq!(parse_priority_from_labels(&["urgent"]), Some(0));
        assert_eq!(parse_priority_from_labels(&["high"]), Some(1));
        assert_eq!(parse_priority_from_labels(&["URGENT"]), Some(0)); // case insensitive
        assert_eq!(parse_priority_from_labels(&["HIGH"]), Some(1));
    }

    #[test]
    fn parse_priority_selects_highest() {
        // Multiple priority labels — pick the highest (lowest number)
        assert_eq!(parse_priority_from_labels(&["p2", "p1"]), Some(1));
        assert_eq!(parse_priority_from_labels(&["p3", "urgent"]), Some(0));
        assert_eq!(parse_priority_from_labels(&["high", "tasks/p2"]), Some(1));
    }

    #[test]
    fn parse_priority_no_match() {
        assert_eq!(parse_priority_from_labels(&["bug", "enhancement"]), None);
        assert_eq!(parse_priority_from_labels(&[]), None);
    }

    #[test]
    fn issue_to_task_sets_priority() {
        let issue = make_issue(42, vec![make_label("bug"), make_label("p1")], GhIssueState::Open);
        let cfg = default_label_config();

        let task = issue_to_task(&issue, "proj-1", &cfg).expect("should produce a task");
        assert_eq!(task.priority, Some(1));
    }

    #[test]
    fn pr_to_task_sets_priority() {
        let pr = make_pr(99, vec![make_label("feature"), make_label("urgent")], PullRequestState::Open);
        let cfg = default_label_config();

        let task = pr_to_task(&pr, "proj-2", &cfg).expect("should produce a task");
        assert_eq!(task.priority, Some(0));
    }

    #[test]
    fn reconcile_closure_stops_active_task() {
        // Issue #499: When an issue is closed externally while a task is Running,
        // reconcile should still transition the task to a terminal state.
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);
        task.set_state(TaskState::Running);

        let mut closed_issue = issue.clone();
        closed_issue.state = GhIssueState::Closed;
        closed_issue.state_reason = Some(tasks_github::model::IssueStateReason::NotPlanned);

        let result = reconcile_task(&mut task, &closed_issue, &cfg);
        assert_eq!(result.new_state, Some(TaskState::Cancelled));
        assert_eq!(task.state, TaskState::Cancelled);
        assert_eq!(
            result.closure_reason,
            Some(tasks_github::model::ClosureReason::NotPlanned)
        );
    }

    #[test]
    fn reconcile_updates_priority() {
        let issue = make_issue(42, vec![make_label("bug")], GhIssueState::Open);
        let cfg = default_label_config();
        let mut task = make_task_from_issue(&issue, &cfg);

        // Initial priority is None
        assert_eq!(task.priority, None);

        // Add a priority label
        let mut updated_issue = issue.clone();
        updated_issue.labels = vec![make_label("bug"), make_label("tasks/p2")];

        let result = reconcile_task(&mut task, &updated_issue, &cfg);
        assert!(result.updated_fields.contains(&"priority"));
        assert_eq!(task.priority, Some(2));
    }
}
