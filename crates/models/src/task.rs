//! Task model — spec Section 5.1, 5.2, 5.3.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Failure classification — spec Section 13.1.
///
/// The system categorizes failures to determine the appropriate response:
/// - Transient: temporary problems likely to resolve on retry
/// - Deterministic: problems that will recur with the same inputs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureType {
    /// Temporary problems: rate limits, network issues, resource exhaustion, OOM kills.
    Transient,
    /// Persistent problems: code errors, invalid config, missing dependencies.
    Deterministic,
}

/// Detailed failure information — spec Section 13.4.
///
/// Captured when a session fails to provide context for debugging and retries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInfo {
    /// Exit code from the agent process, if available.
    pub exit_code: Option<i32>,
    /// Signal that terminated the process, if applicable (e.g., "9" or "SIGKILL").
    pub signal: Option<String>,
    /// How long the session ran before failing (seconds).
    pub duration_secs: u64,
    /// Last lines of stderr output (rolling buffer, max 50 lines).
    pub stderr_tail: Vec<String>,
    /// Classification of the failure.
    pub failure_type: FailureType,
    /// Human-readable summary of the failure.
    pub summary: String,
}

/// Origin reference for a task (spec Section 5.1 `source` field).
///
/// A task may originate from a GitHub issue, a GitHub PR, or be created
/// internally by the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskSource {
    GithubIssue {
        owner: String,
        repo: String,
        number: u64,
    },
    GithubPr {
        owner: String,
        repo: String,
        number: u64,
    },
    Internal,
}

/// Task states — spec Section 5.2.
///
/// These map 1:1 to the `task:state:*` event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// No agent slot available / max concurrency reached.
    Waiting,
    /// Waiting on another task to finish.
    Blocked,
    /// Agent is actively working.
    Running,
    /// Agent is waiting on human or orchestrator for input.
    Question,
    /// Agent done, CI/deterministic testing running.
    Testing,
    /// Implementation complete, changes submitted to merge queue.
    AwaitingMerge,
    /// Merge conflict needs resolution.
    Conflict,
    /// Changes requested on PR — needs work before re-evaluation.
    /// This state supersedes Waiting for dispatch priority.
    ChangesRequested,
    /// Task finished successfully.
    Completed,
    /// Task failed.
    Failed,
    /// Task was cancelled.
    Cancelled,
}

impl TaskState {
    /// Whether this is a terminal state (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Check whether transitioning from this state to `target` is valid.
    ///
    /// The valid transition map encodes the task lifecycle:
    /// - Tasks start in Waiting and flow through active states toward terminal states.
    /// - Terminal states (Completed, Cancelled) allow no outbound transitions.
    /// - Failed allows only a retry back to Waiting.
    /// - Every non-terminal state can transition to Cancelled (operator abort).
    pub fn can_transition_to(&self, target: &TaskState) -> bool {
        use TaskState::*;
        matches!(
            (self, target),
            // Waiting: dispatch to Running, or discover a dependency (Blocked), or cancel
            (Waiting, Running | Blocked | Cancelled)
            // Blocked: dependency resolved back to Waiting, or cancel
            | (Blocked, Waiting | Cancelled)
            // Running: the richest set — agent can ask a question, finish testing,
            // submit for merge, hit a conflict, get changes requested, complete,
            // fail, be cancelled, or retry back to Waiting on session failure (spec §14.3)
            | (Running, Question | Testing | AwaitingMerge | Conflict
                      | ChangesRequested | Completed | Failed | Cancelled | Waiting)
            // Question: answer received returns to Running, or fail/cancel,
            // or retry to Waiting on restart recovery of orphaned session (spec §14.3)
            | (Question, Running | Waiting | Failed | Cancelled)
            // Testing: tests pass (back to Running for more work, or AwaitingMerge),
            // tests fail, cancel, or retry to Waiting on restart recovery (spec §14.3)
            | (Testing, Running | AwaitingMerge | Waiting | Failed | Cancelled)
            // AwaitingMerge: merged (Completed), conflict, changes requested,
            // fail, or cancel
            | (AwaitingMerge, Completed | Conflict | ChangesRequested | Failed | Cancelled)
            // Conflict: agent retries (Running), or reviewer requests changes,
            // fail, or cancel
            | (Conflict, Running | ChangesRequested | Failed | Cancelled)
            // ChangesRequested: agent picks it back up (Running), or park it
            // (Waiting), fail, or cancel
            | (ChangesRequested, Running | Waiting | Failed | Cancelled)
            // Failed: only valid outbound is Waiting (retry)
            | (Failed, Waiting)
            // Completed, Cancelled: terminal — no transitions out
        )
    }
}

/// A task — the internal representation of a unit of work.
///
/// Spec Section 5.1. Every field here maps to a field in the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Internal task ID.
    pub id: String,
    /// Origin reference (GitHub issue/PR or internal).
    pub source: TaskSource,
    pub title: String,
    /// May be null for tasks created without a description.
    pub description: Option<String>,
    /// Current task state (spec Section 5.2).
    pub state: TaskState,
    /// Parent task ID, if this is a sub-task (spec Section 5.3).
    pub parent_id: Option<String>,
    /// Tasks that must complete before this one can proceed.
    pub blocked_by: Vec<String>,
    /// Project ID this task belongs to.
    pub project: String,
    pub labels: Vec<String>,
    /// Lower numbers are higher priority.
    pub priority: Option<i32>,
    /// Active session ID, if any.
    pub session_id: Option<String>,
    /// Workspace ID, if provisioned.
    pub workspace_id: Option<String>,
    /// Number of times this task has been retried (spec §13.2).
    pub retry_count: u32,
    /// When the most recent failure occurred (spec §13.2).
    pub last_failure_at: Option<DateTime<Utc>>,
    /// Detailed information about the last failure (spec §13.4).
    pub last_failure: Option<FailureInfo>,
    /// When the source (GitHub issue/PR) was created. Used for dispatch ordering.
    pub source_created_at: Option<DateTime<Utc>>,
    /// GitHub issue/PR number. Used for deterministic dispatch ordering within a project.
    /// Lower numbers are older and should be processed first among otherwise equal tasks.
    pub source_number: Option<u64>,
    /// Last activity timestamp for stale workspace detection (spec §10.3).
    /// Updated when the task transitions to/from active states.
    pub last_activity_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Feedback from orchestrator after PR rejection (issue #423).
    /// Delivered to the agent when the task is re-dispatched.
    /// Cleared after dispatch so stale feedback isn't repeated.
    pub rejection_feedback: Option<String>,
}

impl Task {
    /// Create a new task in the Waiting state.
    pub fn new(
        id: impl Into<String>,
        source: TaskSource,
        title: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            source,
            title: title.into(),
            description: None,
            state: TaskState::Waiting,
            parent_id: None,
            blocked_by: Vec::new(),
            project: project.into(),
            labels: Vec::new(),
            priority: None,
            session_id: None,
            workspace_id: None,
            retry_count: 0,
            last_failure_at: None,
            last_failure: None,
            source_created_at: None,
            source_number: None,
            last_activity_at: None,
            created_at: now,
            updated_at: now,
            rejection_feedback: None,
        }
    }

    /// Transition to a new state, updating the timestamp.
    ///
    /// Logs a warning when the transition is not in the expected state machine,
    /// but still allows it — we don't hard-fail because there may be edge cases
    /// we haven't accounted for yet. The warnings let us identify them.
    ///
    /// Also updates `last_activity_at` when entering or leaving active states
    /// (Running, Question, Testing) for stale workspace detection (spec §10.3).
    pub fn set_state(&mut self, state: TaskState) {
        if !self.state.can_transition_to(&state) {
            tracing::warn!(
                task_id = %self.id,
                from = ?self.state,
                to = ?state,
                "invalid task state transition"
            );
        }

        let now = Utc::now();

        // Update last_activity_at when entering/leaving active states
        let was_active = matches!(
            self.state,
            TaskState::Running | TaskState::Question | TaskState::Testing
        );
        let is_active = matches!(
            state,
            TaskState::Running | TaskState::Question | TaskState::Testing
        );

        if was_active || is_active {
            self.last_activity_at = Some(now);
        }

        self.state = state;
        self.updated_at = now;
    }
}

/// Validate a `blocked_by` list for a given task against the full task set.
///
/// Returns a filtered list with invalid entries removed:
/// - Self-references (task blocking itself)
/// - References to nonexistent tasks
/// - References that would create circular dependency chains
///
/// Warnings are logged for each removed entry.
pub fn validate_blocked_by(
    task_id: &str,
    proposed: &[String],
    tasks: &HashMap<String, Task>,
) -> Vec<String> {
    let mut valid = Vec::with_capacity(proposed.len());

    for dep_id in proposed {
        // Reject self-references.
        if dep_id == task_id {
            tracing::warn!(
                task_id = %task_id,
                dep_id = %dep_id,
                "blocked_by: removing self-reference"
            );
            continue;
        }

        // Reject references to nonexistent tasks.
        if !tasks.contains_key(dep_id.as_str()) {
            tracing::warn!(
                task_id = %task_id,
                dep_id = %dep_id,
                "blocked_by: removing reference to nonexistent task"
            );
            continue;
        }

        // Reject if adding this dependency would create a cycle.
        // Walk the blocked_by chain from dep_id; if we reach task_id, it's circular.
        if creates_cycle(task_id, dep_id, tasks) {
            tracing::warn!(
                task_id = %task_id,
                dep_id = %dep_id,
                "blocked_by: removing dependency that would create a cycle"
            );
            continue;
        }

        valid.push(dep_id.clone());
    }

    valid
}

/// Check whether adding an edge task_id → dep_id would create a cycle.
///
/// Performs a DFS from `dep_id` following each task's `blocked_by` edges.
/// If we can reach `task_id`, the new edge would close a cycle.
fn creates_cycle(task_id: &str, dep_id: &str, tasks: &HashMap<String, Task>) -> bool {
    let mut visited = HashSet::new();
    let mut stack = vec![dep_id];

    while let Some(current) = stack.pop() {
        if current == task_id {
            return true;
        }
        if !visited.insert(current) {
            continue;
        }
        if let Some(task) = tasks.get(current) {
            for next in &task.blocked_by {
                stack.push(next.as_str());
            }
        }
    }

    false
}

impl FailureInfo {
    /// Classify a session failure based on exit code, signal, and duration.
    ///
    /// Per spec §13.1:
    /// - Transient: rate limits, network issues, resource exhaustion, OOM kills (signal 9)
    /// - Deterministic: code errors, invalid config, missing dependencies
    /// - "Making progress" = agent ran >= progress_threshold_secs
    pub fn classify(
        exit_code: Option<i32>,
        signal: Option<String>,
        duration_secs: u64,
        progress_threshold_secs: u64,
        stderr_tail: Vec<String>,
    ) -> Self {
        let made_progress = duration_secs >= progress_threshold_secs;

        // Parse signal string to number if possible
        let signal_num = signal.as_ref().and_then(|s| {
            s.parse::<i32>().ok().or_else(|| {
                // Try to parse common signal names
                match s.to_uppercase().as_str() {
                    "SIGKILL" | "KILL" => Some(9),
                    "SIGTERM" | "TERM" => Some(15),
                    "SIGINT" | "INT" => Some(2),
                    "SIGSEGV" | "SEGV" => Some(11),
                    "SIGABRT" | "ABRT" => Some(6),
                    _ => None,
                }
            })
        });

        // Determine failure type based on exit code, signal, and progress
        let (failure_type, summary) = match (exit_code, signal_num) {
            // OOM kill (signal 9) is transient
            (_, Some(9)) => (
                FailureType::Transient,
                "Process killed by OOM killer (signal 9)".to_string(),
            ),
            // SIGTERM (15) from timeout is transient
            (_, Some(15)) => (
                FailureType::Transient,
                "Process terminated by signal 15 (SIGTERM)".to_string(),
            ),
            // If progress was made, treat as transient (may have hit edge case)
            _ if made_progress => (
                FailureType::Transient,
                format!("Session ran for {duration_secs}s before failing"),
            ),
            // Exit code 1 without progress is likely a code error
            (Some(1), _) => (
                FailureType::Deterministic,
                "Process exited with code 1 (error)".to_string(),
            ),
            // Exit code 2 is often invalid arguments/config
            (Some(2), _) => (
                FailureType::Deterministic,
                "Process exited with code 2 (invalid arguments)".to_string(),
            ),
            // Exit code 127 is command not found
            (Some(127), _) => (
                FailureType::Deterministic,
                "Process exited with code 127 (command not found)".to_string(),
            ),
            // Exit code 126 is permission denied
            (Some(126), _) => (
                FailureType::Deterministic,
                "Process exited with code 126 (permission denied)".to_string(),
            ),
            // Other non-zero exits without progress are deterministic
            (Some(code), _) if code != 0 => (
                FailureType::Deterministic,
                format!("Process exited with code {code}"),
            ),
            // Unknown failure
            _ => (
                FailureType::Transient,
                "Unknown failure (no exit code or signal)".to_string(),
            ),
        };

        Self {
            exit_code,
            signal,
            duration_secs,
            stderr_tail,
            failure_type,
            summary,
        }
    }

    /// Check for transient patterns in stderr (rate limits, network errors).
    ///
    /// Call this after initial classification to potentially upgrade
    /// a deterministic failure to transient if stderr indicates a
    /// recoverable issue.
    pub fn check_stderr_for_transient_patterns(&mut self) {
        let stderr_text = self.stderr_tail.join("\n").to_lowercase();

        // Patterns that indicate transient failures
        let transient_patterns = [
            "rate limit",
            "rate_limit",
            "too many requests",
            "429",
            "timeout",
            "timed out",
            "connection refused",
            "connection reset",
            "network unreachable",
            "dns",
            "econnrefused",
            "econnreset",
            "etimedout",
            "resource temporarily unavailable",
            "out of memory",
            "oom",
            "no space left on device",
            "disk quota exceeded",
        ];

        for pattern in transient_patterns {
            if stderr_text.contains(pattern) {
                self.failure_type = FailureType::Transient;
                self.summary = format!(
                    "{} (detected transient pattern: {})",
                    self.summary, pattern
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All states in the enum, used for exhaustive testing.
    const ALL_STATES: [TaskState; 11] = [
        TaskState::Waiting,
        TaskState::Blocked,
        TaskState::Running,
        TaskState::Question,
        TaskState::Testing,
        TaskState::AwaitingMerge,
        TaskState::Conflict,
        TaskState::ChangesRequested,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Cancelled,
    ];

    // ---- is_terminal ----

    #[test]
    fn terminal_states() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(TaskState::Cancelled.is_terminal());
    }

    #[test]
    fn non_terminal_states() {
        let non_terminal = [
            TaskState::Waiting,
            TaskState::Blocked,
            TaskState::Running,
            TaskState::Question,
            TaskState::Testing,
            TaskState::AwaitingMerge,
            TaskState::Conflict,
            TaskState::ChangesRequested,
        ];
        for state in non_terminal {
            assert!(!state.is_terminal(), "{state:?} should not be terminal");
        }
    }

    // ---- can_transition_to: valid transitions ----

    #[test]
    fn waiting_valid_transitions() {
        let valid = [TaskState::Running, TaskState::Blocked, TaskState::Cancelled];
        for target in valid {
            assert!(
                TaskState::Waiting.can_transition_to(&target),
                "Waiting -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn blocked_valid_transitions() {
        let valid = [TaskState::Waiting, TaskState::Cancelled];
        for target in valid {
            assert!(
                TaskState::Blocked.can_transition_to(&target),
                "Blocked -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn running_valid_transitions() {
        let valid = [
            TaskState::Question,
            TaskState::Testing,
            TaskState::AwaitingMerge,
            TaskState::Conflict,
            TaskState::ChangesRequested,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Cancelled,
            TaskState::Waiting, // session retry (spec §14.3)
        ];
        for target in valid {
            assert!(
                TaskState::Running.can_transition_to(&target),
                "Running -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn question_valid_transitions() {
        let valid = [
            TaskState::Running,
            TaskState::Waiting, // restart recovery of orphaned session (spec §14.3)
            TaskState::Failed,
            TaskState::Cancelled,
        ];
        for target in valid {
            assert!(
                TaskState::Question.can_transition_to(&target),
                "Question -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn testing_valid_transitions() {
        let valid = [
            TaskState::Running,
            TaskState::AwaitingMerge,
            TaskState::Waiting, // restart recovery of orphaned session (spec §14.3)
            TaskState::Failed,
            TaskState::Cancelled,
        ];
        for target in valid {
            assert!(
                TaskState::Testing.can_transition_to(&target),
                "Testing -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn awaiting_merge_valid_transitions() {
        let valid = [
            TaskState::Completed,
            TaskState::Conflict,
            TaskState::ChangesRequested,
            TaskState::Failed,
            TaskState::Cancelled,
        ];
        for target in valid {
            assert!(
                TaskState::AwaitingMerge.can_transition_to(&target),
                "AwaitingMerge -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn conflict_valid_transitions() {
        let valid = [
            TaskState::Running,
            TaskState::ChangesRequested,
            TaskState::Failed,
            TaskState::Cancelled,
        ];
        for target in valid {
            assert!(
                TaskState::Conflict.can_transition_to(&target),
                "Conflict -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn changes_requested_valid_transitions() {
        let valid = [
            TaskState::Running,
            TaskState::Waiting,
            TaskState::Failed,
            TaskState::Cancelled,
        ];
        for target in valid {
            assert!(
                TaskState::ChangesRequested.can_transition_to(&target),
                "ChangesRequested -> {target:?} should be valid"
            );
        }
    }

    #[test]
    fn failed_valid_transitions() {
        assert!(
            TaskState::Failed.can_transition_to(&TaskState::Waiting),
            "Failed -> Waiting (retry) should be valid"
        );
    }

    // ---- can_transition_to: terminal states reject all outbound ----

    #[test]
    fn completed_has_no_outbound_transitions() {
        for target in ALL_STATES {
            assert!(
                !TaskState::Completed.can_transition_to(&target),
                "Completed -> {target:?} should be invalid"
            );
        }
    }

    #[test]
    fn cancelled_has_no_outbound_transitions() {
        for target in ALL_STATES {
            assert!(
                !TaskState::Cancelled.can_transition_to(&target),
                "Cancelled -> {target:?} should be invalid"
            );
        }
    }

    #[test]
    fn failed_rejects_all_except_waiting() {
        for target in ALL_STATES {
            if target == TaskState::Waiting {
                continue;
            }
            assert!(
                !TaskState::Failed.can_transition_to(&target),
                "Failed -> {target:?} should be invalid"
            );
        }
    }

    // ---- can_transition_to: specific invalid transitions ----

    #[test]
    fn waiting_cannot_go_to_completed() {
        assert!(!TaskState::Waiting.can_transition_to(&TaskState::Completed));
    }

    #[test]
    fn blocked_cannot_go_to_running() {
        assert!(!TaskState::Blocked.can_transition_to(&TaskState::Running));
    }

    #[test]
    fn self_transitions_are_invalid() {
        // No state should transition to itself
        for state in ALL_STATES {
            assert!(
                !state.can_transition_to(&state),
                "{state:?} -> {state:?} (self-transition) should be invalid"
            );
        }
    }

    // ---- set_state integration ----

    #[test]
    fn set_state_updates_state_and_timestamp() {
        let mut task = Task::new("t1", TaskSource::Internal, "test task", "proj1");
        let before = task.updated_at;

        // Small delay not needed — Utc::now() has sub-microsecond resolution
        task.set_state(TaskState::Running);

        assert_eq!(task.state, TaskState::Running);
        assert!(task.updated_at >= before);
    }

    #[test]
    fn set_state_updates_last_activity_on_active_transition() {
        let mut task = Task::new("t1", TaskSource::Internal, "test task", "proj1");
        assert!(task.last_activity_at.is_none());

        task.set_state(TaskState::Running);
        assert!(task.last_activity_at.is_some());
    }

    #[test]
    fn set_state_allows_invalid_transition_without_panic() {
        // set_state should warn but not panic on invalid transitions
        let mut task = Task::new("t1", TaskSource::Internal, "test task", "proj1");
        task.state = TaskState::Completed;

        // Completed -> Running is invalid, but should not panic
        task.set_state(TaskState::Running);
        assert_eq!(task.state, TaskState::Running);
    }

    // ---- validate_blocked_by ----

    fn make_tasks(ids: &[&str]) -> HashMap<String, Task> {
        ids.iter()
            .map(|id| {
                (
                    id.to_string(),
                    Task::new(*id, TaskSource::Internal, "task", "proj"),
                )
            })
            .collect()
    }

    #[test]
    fn validate_rejects_self_reference() {
        let tasks = make_tasks(&["t1", "t2"]);
        let result = validate_blocked_by("t1", &["t1".into(), "t2".into()], &tasks);
        assert_eq!(result, vec!["t2"]);
    }

    #[test]
    fn validate_rejects_nonexistent_task() {
        let tasks = make_tasks(&["t1", "t2"]);
        let result = validate_blocked_by("t1", &["t2".into(), "t99".into()], &tasks);
        assert_eq!(result, vec!["t2"]);
    }

    #[test]
    fn validate_rejects_direct_cycle() {
        // t1 blocked_by t2, t2 blocked_by t1 → adding t2 to t1 creates cycle
        let mut tasks = make_tasks(&["t1", "t2"]);
        tasks.get_mut("t2").unwrap().blocked_by = vec!["t1".into()];

        let result = validate_blocked_by("t1", &["t2".into()], &tasks);
        assert!(result.is_empty(), "cycle through t2→t1 should be rejected");
    }

    #[test]
    fn validate_rejects_transitive_cycle() {
        // t3 → t2 → t1 (existing), adding t1 blocked_by t3 would close the cycle
        let mut tasks = make_tasks(&["t1", "t2", "t3"]);
        tasks.get_mut("t2").unwrap().blocked_by = vec!["t1".into()];
        tasks.get_mut("t3").unwrap().blocked_by = vec!["t2".into()];

        let result = validate_blocked_by("t1", &["t3".into()], &tasks);
        assert!(result.is_empty(), "transitive cycle t3→t2→t1 should be rejected");
    }

    #[test]
    fn validate_accepts_valid_dependency() {
        let tasks = make_tasks(&["t1", "t2", "t3"]);
        let result = validate_blocked_by("t1", &["t2".into(), "t3".into()], &tasks);
        assert_eq!(result, vec!["t2", "t3"]);
    }

    #[test]
    fn validate_empty_blocked_by() {
        let tasks = make_tasks(&["t1"]);
        let result = validate_blocked_by("t1", &[], &tasks);
        assert!(result.is_empty());
    }
}
