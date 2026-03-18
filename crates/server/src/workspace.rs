//! Workspace cleanup policies — spec §10.3.
//!
//! Workspaces are cleaned up when they are no longer needed:
//! - PR merged → delete workspace
//! - Task completed/cancelled/failed → eligible for cleanup
//! - Stale/idle → workspaces with no activity for configurable period
//!
//! Event logs are retained independently (spec §10.4).

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::model::task::{Task, TaskState};
use crate::model::merge_queue::{MergeQueueEntry, MergeStatus};

/// Default stale workspace threshold: 7 days (spec §10.3).
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 7 * 24 * 60 * 60;

/// Default cleanup scan interval: 1 hour.
pub const DEFAULT_CLEANUP_INTERVAL_SECS: u64 = 60 * 60;

/// Workspace cleanup configuration.
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// Time after which an idle workspace is considered stale (spec §10.3).
    pub stale_threshold: Duration,
    /// Interval between cleanup scans.
    pub cleanup_interval: Duration,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            stale_threshold: Duration::from_secs(DEFAULT_STALE_THRESHOLD_SECS),
            cleanup_interval: Duration::from_secs(DEFAULT_CLEANUP_INTERVAL_SECS),
        }
    }
}

impl CleanupConfig {
    /// Create a new CleanupConfig from environment variables.
    ///
    /// - `TASKS_WORKSPACE_STALE_THRESHOLD` — Idle threshold in seconds (default: 7 days)
    /// - `TASKS_CLEANUP_INTERVAL` — Scan interval in seconds (default: 1 hour)
    pub fn from_env() -> Self {
        let stale_threshold = std::env::var("TASKS_WORKSPACE_STALE_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_STALE_THRESHOLD_SECS));

        let cleanup_interval = std::env::var("TASKS_CLEANUP_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_CLEANUP_INTERVAL_SECS));

        Self {
            stale_threshold,
            cleanup_interval,
        }
    }
}

/// Reason for workspace cleanup eligibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupReason {
    /// Task transitioned to a terminal state (Completed, Failed, Cancelled).
    TerminalState(TaskState),
    /// PR was merged via the merge queue.
    PrMerged,
    /// Workspace has been idle for longer than the stale threshold.
    Stale { idle_duration: Duration },
}

/// A workspace eligible for cleanup.
#[derive(Debug, Clone)]
pub struct CleanupCandidate {
    /// The task ID owning this workspace.
    pub task_id: String,
    /// The workspace ID to clean up.
    pub workspace_id: String,
    /// Why this workspace is eligible for cleanup.
    pub reason: CleanupReason,
}

/// Evaluate whether a task's workspace is eligible for cleanup due to terminal state.
///
/// Per spec §10.3: Task completed/cancelled/failed → eligible for cleanup.
pub fn is_terminal_state_cleanup(task: &Task) -> Option<CleanupCandidate> {
    if !task.state.is_terminal() {
        return None;
    }

    // Must have a workspace_id to clean up
    let workspace_id = task.workspace_id.as_ref()?;

    Some(CleanupCandidate {
        task_id: task.id.clone(),
        workspace_id: workspace_id.clone(),
        reason: CleanupReason::TerminalState(task.state),
    })
}

/// Evaluate whether a task's workspace is eligible for cleanup due to PR merge.
///
/// Per spec §10.3: PR merged → delete workspace.
pub fn is_pr_merged_cleanup(task: &Task, entry: &MergeQueueEntry) -> Option<CleanupCandidate> {
    if entry.status != MergeStatus::Merged {
        return None;
    }

    // Entry must be for this task
    if entry.task_id != task.id {
        return None;
    }

    // Must have a workspace_id to clean up
    let workspace_id = task.workspace_id.as_ref()?;

    Some(CleanupCandidate {
        task_id: task.id.clone(),
        workspace_id: workspace_id.clone(),
        reason: CleanupReason::PrMerged,
    })
}

/// Evaluate whether a task's workspace is stale (idle beyond threshold).
///
/// Per spec §10.3: Workspaces with no active session for a configurable
/// period are eligible for cleanup.
///
/// A workspace is considered stale if:
/// - The task is NOT in an active state (Running, Question, Testing)
/// - The task has `last_activity_at` set and it's older than the threshold
/// - OR the task has no `last_activity_at` but `updated_at` is older than threshold
pub fn is_stale_cleanup(task: &Task, now: DateTime<Utc>, threshold: Duration) -> Option<CleanupCandidate> {
    // Don't clean up active tasks
    if matches!(
        task.state,
        TaskState::Running | TaskState::Question | TaskState::Testing
    ) {
        return None;
    }

    // Must have a workspace_id to clean up
    let workspace_id = task.workspace_id.as_ref()?;

    // Determine the last activity time
    let last_activity = task.last_activity_at.unwrap_or(task.updated_at);
    let idle_duration = (now - last_activity).to_std().ok()?;

    if idle_duration < threshold {
        return None;
    }

    Some(CleanupCandidate {
        task_id: task.id.clone(),
        workspace_id: workspace_id.clone(),
        reason: CleanupReason::Stale { idle_duration },
    })
}

/// Find all tasks eligible for cleanup based on current state.
///
/// This performs a single pass over all tasks to find:
/// - Tasks in terminal states (Completed, Failed, Cancelled)
/// - Tasks with workspaces that are stale (idle beyond threshold)
///
/// PR merge cleanup is handled separately through merge queue events.
pub fn find_cleanup_candidates(
    tasks: impl Iterator<Item = Task>,
    now: DateTime<Utc>,
    stale_threshold: Duration,
) -> Vec<CleanupCandidate> {
    let mut candidates = Vec::new();

    for task in tasks {
        // Skip tasks without workspaces
        if task.workspace_id.is_none() {
            continue;
        }

        // Check terminal state first (takes priority)
        if let Some(candidate) = is_terminal_state_cleanup(&task) {
            candidates.push(candidate);
            continue;
        }

        // Check for stale workspaces
        if let Some(candidate) = is_stale_cleanup(&task, now, stale_threshold) {
            candidates.push(candidate);
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::task::TaskSource;
    use chrono::TimeDelta;

    fn make_task(id: &str, state: TaskState, workspace_id: Option<&str>) -> Task {
        let mut task = Task::new(id, TaskSource::Internal, "Test task", "project-1");
        task.state = state;
        task.workspace_id = workspace_id.map(String::from);
        task
    }

    #[test]
    fn terminal_state_cleanup_completed() {
        let task = make_task("t1", TaskState::Completed, Some("ws-1"));
        let candidate = is_terminal_state_cleanup(&task);
        assert!(candidate.is_some());
        let c = candidate.unwrap();
        assert_eq!(c.task_id, "t1");
        assert_eq!(c.workspace_id, "ws-1");
        assert_eq!(c.reason, CleanupReason::TerminalState(TaskState::Completed));
    }

    #[test]
    fn terminal_state_cleanup_failed() {
        let task = make_task("t1", TaskState::Failed, Some("ws-1"));
        let candidate = is_terminal_state_cleanup(&task);
        assert!(candidate.is_some());
        assert_eq!(candidate.unwrap().reason, CleanupReason::TerminalState(TaskState::Failed));
    }

    #[test]
    fn terminal_state_cleanup_cancelled() {
        let task = make_task("t1", TaskState::Cancelled, Some("ws-1"));
        let candidate = is_terminal_state_cleanup(&task);
        assert!(candidate.is_some());
        assert_eq!(candidate.unwrap().reason, CleanupReason::TerminalState(TaskState::Cancelled));
    }

    #[test]
    fn terminal_state_no_cleanup_running() {
        let task = make_task("t1", TaskState::Running, Some("ws-1"));
        assert!(is_terminal_state_cleanup(&task).is_none());
    }

    #[test]
    fn terminal_state_no_cleanup_without_workspace() {
        let task = make_task("t1", TaskState::Completed, None);
        assert!(is_terminal_state_cleanup(&task).is_none());
    }

    #[test]
    fn stale_cleanup_idle_workspace() {
        let mut task = make_task("t1", TaskState::Waiting, Some("ws-1"));
        let now = Utc::now();
        // Set last_activity_at to 8 days ago
        task.last_activity_at = Some(now - TimeDelta::days(8));

        let threshold = Duration::from_secs(7 * 24 * 60 * 60); // 7 days
        let candidate = is_stale_cleanup(&task, now, threshold);
        assert!(candidate.is_some());
        let c = candidate.unwrap();
        assert_eq!(c.task_id, "t1");
        matches!(c.reason, CleanupReason::Stale { .. });
    }

    #[test]
    fn stale_cleanup_recent_activity() {
        let mut task = make_task("t1", TaskState::Waiting, Some("ws-1"));
        let now = Utc::now();
        // Set last_activity_at to 1 day ago
        task.last_activity_at = Some(now - TimeDelta::days(1));

        let threshold = Duration::from_secs(7 * 24 * 60 * 60); // 7 days
        assert!(is_stale_cleanup(&task, now, threshold).is_none());
    }

    #[test]
    fn stale_cleanup_not_active_task() {
        let mut task = make_task("t1", TaskState::Running, Some("ws-1"));
        let now = Utc::now();
        task.last_activity_at = Some(now - TimeDelta::days(8));

        let threshold = Duration::from_secs(7 * 24 * 60 * 60);
        // Running tasks should not be cleaned up as stale
        assert!(is_stale_cleanup(&task, now, threshold).is_none());
    }

    #[test]
    fn stale_cleanup_uses_updated_at_fallback() {
        let mut task = make_task("t1", TaskState::Waiting, Some("ws-1"));
        let now = Utc::now();
        // No last_activity_at, but updated_at is 8 days ago
        task.last_activity_at = None;
        task.updated_at = now - TimeDelta::days(8);

        let threshold = Duration::from_secs(7 * 24 * 60 * 60);
        let candidate = is_stale_cleanup(&task, now, threshold);
        assert!(candidate.is_some());
    }

    #[test]
    fn pr_merged_cleanup() {
        let task = make_task("t1", TaskState::AwaitingMerge, Some("ws-1"));
        let mut entry = MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1");
        entry.status = MergeStatus::Merged;

        let candidate = is_pr_merged_cleanup(&task, &entry);
        assert!(candidate.is_some());
        let c = candidate.unwrap();
        assert_eq!(c.task_id, "t1");
        assert_eq!(c.reason, CleanupReason::PrMerged);
    }

    #[test]
    fn pr_not_merged_no_cleanup() {
        let task = make_task("t1", TaskState::AwaitingMerge, Some("ws-1"));
        let entry = MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1");
        // status is Pending by default
        assert!(is_pr_merged_cleanup(&task, &entry).is_none());
    }

    #[test]
    fn find_cleanup_candidates_mixed() {
        let now = Utc::now();
        let threshold = Duration::from_secs(7 * 24 * 60 * 60);

        let mut tasks = vec![
            make_task("t1", TaskState::Completed, Some("ws-1")),          // terminal
            make_task("t2", TaskState::Running, Some("ws-2")),            // active, skip
            make_task("t3", TaskState::Waiting, None),                     // no workspace, skip
            make_task("t4", TaskState::Failed, Some("ws-4")),             // terminal
        ];

        // Make t5 stale
        let mut t5 = make_task("t5", TaskState::Waiting, Some("ws-5"));
        t5.last_activity_at = Some(now - TimeDelta::days(10));
        tasks.push(t5);

        let candidates = find_cleanup_candidates(tasks.into_iter(), now, threshold);

        assert_eq!(candidates.len(), 3); // t1, t4 (terminal), t5 (stale)

        let task_ids: Vec<_> = candidates.iter().map(|c| c.task_id.as_str()).collect();
        assert!(task_ids.contains(&"t1"));
        assert!(task_ids.contains(&"t4"));
        assert!(task_ids.contains(&"t5"));
    }

    #[test]
    fn cleanup_config_defaults() {
        let config = CleanupConfig::default();
        assert_eq!(config.stale_threshold, Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(config.cleanup_interval, Duration::from_secs(60 * 60));
    }
}
