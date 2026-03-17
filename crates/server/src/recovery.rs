//! Restart recovery for orphaned sessions (spec §13.3).
//!
//! When the server restarts, sessions that were active (Running/Question/Testing)
//! may have lost their containers. This module detects orphaned sessions and
//! transitions them appropriately based on retry budget.

use std::collections::HashMap;

use runtime::ContainerRuntime;
use tracing::{info, warn};

use crate::model::task::{Task, TaskState};

/// Default maximum retry count (spec §14.1 default).
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Result of recovering orphaned sessions.
#[derive(Debug, Default)]
pub struct RecoveryResult {
    /// Task IDs that were transitioned to Waiting for retry.
    pub retried: Vec<String>,
    /// Task IDs that were transitioned to Failed (retries exhausted).
    pub failed: Vec<String>,
    /// Task IDs whose containers still exist (no recovery needed).
    pub alive: Vec<String>,
}

/// Check if a task is in an "active session" state that requires a container.
fn is_active_session_state(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Running | TaskState::Question | TaskState::Testing
    )
}

/// Detect orphaned sessions and determine recovery actions.
///
/// For each task in an active session state (Running/Question/Testing):
/// - If the container still exists, no action needed
/// - If the container is gone and retry_count < max_retries, schedule for retry
/// - If the container is gone and retry_count >= max_retries, mark as failed
///
/// Returns the recovery plan. The caller is responsible for applying state
/// transitions and emitting events.
pub async fn detect_orphaned_sessions<R: ContainerRuntime>(
    tasks: &HashMap<String, Task>,
    runtime: &R,
    max_retries: u32,
) -> RecoveryResult {
    let mut result = RecoveryResult::default();

    // Find all tasks in active session states
    let active_tasks: Vec<&Task> = tasks
        .values()
        .filter(|t| is_active_session_state(t.state))
        .collect();

    if active_tasks.is_empty() {
        return result;
    }

    info!(
        count = active_tasks.len(),
        "checking for orphaned sessions after restart"
    );

    for task in active_tasks {
        // If no session_id, the task is in an inconsistent state — treat as orphaned
        let container_id = match &task.session_id {
            Some(id) => id,
            None => {
                warn!(
                    task_id = %task.id,
                    state = ?task.state,
                    "task in active state but no session_id — treating as orphaned"
                );
                if task.retry_count < max_retries {
                    result.retried.push(task.id.clone());
                } else {
                    result.failed.push(task.id.clone());
                }
                continue;
            }
        };

        // Check if the container still exists
        let exists = match runtime.container_exists(container_id).await {
            Ok(exists) => exists,
            Err(e) => {
                warn!(
                    task_id = %task.id,
                    container_id = %container_id,
                    error = %e,
                    "failed to check container existence — assuming orphaned"
                );
                false
            }
        };

        if exists {
            info!(
                task_id = %task.id,
                container_id = %container_id,
                "container still exists after restart"
            );
            result.alive.push(task.id.clone());
        } else {
            // Container is gone — check retry budget
            if task.retry_count < max_retries {
                info!(
                    task_id = %task.id,
                    container_id = %container_id,
                    retry_count = task.retry_count,
                    max_retries = max_retries,
                    "orphaned session detected — will retry"
                );
                result.retried.push(task.id.clone());
            } else {
                warn!(
                    task_id = %task.id,
                    container_id = %container_id,
                    retry_count = task.retry_count,
                    max_retries = max_retries,
                    "orphaned session detected — retries exhausted, marking failed"
                );
                result.failed.push(task.id.clone());
            }
        }
    }

    info!(
        alive = result.alive.len(),
        retried = result.retried.len(),
        failed = result.failed.len(),
        "orphan recovery complete"
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::task::TaskSource;
    use runtime::ContainerError;
    use std::collections::HashSet;

    /// Mock container runtime for testing.
    struct MockRuntime {
        existing_containers: HashSet<String>,
    }

    impl MockRuntime {
        fn new(existing: &[&str]) -> Self {
            Self {
                existing_containers: existing.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl ContainerRuntime for MockRuntime {
        async fn create(
            &self,
            _config: &runtime::ContainerConfig,
        ) -> Result<String, ContainerError> {
            Ok("mock-container".to_string())
        }

        async fn start(
            &self,
            _container_id: &str,
        ) -> Result<runtime::StdioTransport, ContainerError> {
            unimplemented!("not needed for recovery tests")
        }

        async fn stop(&self, _container_id: &str) -> Result<(), ContainerError> {
            Ok(())
        }

        async fn destroy(&self, _container_id: &str) -> Result<(), ContainerError> {
            Ok(())
        }

        async fn container_exists(&self, container_id: &str) -> Result<bool, ContainerError> {
            Ok(self.existing_containers.contains(container_id))
        }
    }

    fn make_task(id: &str, state: TaskState, session_id: Option<&str>, retry_count: u32) -> Task {
        let mut task = Task::new(id, TaskSource::Internal, "Test task", "project-1");
        task.state = state;
        task.session_id = session_id.map(String::from);
        task.retry_count = retry_count;
        task
    }

    #[tokio::test]
    async fn no_active_sessions_returns_empty() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Waiting, None, 0),
        );
        tasks.insert(
            "t2".to_string(),
            make_task("t2", TaskState::Completed, None, 0),
        );

        let runtime = MockRuntime::new(&[]);
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert!(result.retried.is_empty());
        assert!(result.failed.is_empty());
        assert!(result.alive.is_empty());
    }

    #[tokio::test]
    async fn container_exists_no_recovery_needed() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Running, Some("container-1"), 0),
        );

        let runtime = MockRuntime::new(&["container-1"]);
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert!(result.retried.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(result.alive, vec!["t1"]);
    }

    #[tokio::test]
    async fn orphaned_within_retry_budget() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Running, Some("container-1"), 1),
        );

        let runtime = MockRuntime::new(&[]); // Container doesn't exist
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert_eq!(result.retried, vec!["t1"]);
        assert!(result.failed.is_empty());
        assert!(result.alive.is_empty());
    }

    #[tokio::test]
    async fn orphaned_retries_exhausted() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Running, Some("container-1"), 3),
        );

        let runtime = MockRuntime::new(&[]); // Container doesn't exist
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert!(result.retried.is_empty());
        assert_eq!(result.failed, vec!["t1"]);
        assert!(result.alive.is_empty());
    }

    #[tokio::test]
    async fn no_session_id_treated_as_orphaned() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Running, None, 0),
        );

        let runtime = MockRuntime::new(&[]);
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert_eq!(result.retried, vec!["t1"]);
        assert!(result.failed.is_empty());
        assert!(result.alive.is_empty());
    }

    #[tokio::test]
    async fn question_state_is_active() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Question, Some("container-1"), 0),
        );

        let runtime = MockRuntime::new(&[]); // Container doesn't exist
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert_eq!(result.retried, vec!["t1"]);
    }

    #[tokio::test]
    async fn testing_state_is_active() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Testing, Some("container-1"), 0),
        );

        let runtime = MockRuntime::new(&[]); // Container doesn't exist
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert_eq!(result.retried, vec!["t1"]);
    }

    #[tokio::test]
    async fn mixed_scenarios() {
        let mut tasks = HashMap::new();
        // t1: Running, container exists
        tasks.insert(
            "t1".to_string(),
            make_task("t1", TaskState::Running, Some("container-1"), 0),
        );
        // t2: Running, container gone, can retry
        tasks.insert(
            "t2".to_string(),
            make_task("t2", TaskState::Running, Some("container-2"), 1),
        );
        // t3: Question, container gone, retries exhausted
        tasks.insert(
            "t3".to_string(),
            make_task("t3", TaskState::Question, Some("container-3"), 3),
        );
        // t4: Waiting, not an active session
        tasks.insert(
            "t4".to_string(),
            make_task("t4", TaskState::Waiting, None, 0),
        );

        let runtime = MockRuntime::new(&["container-1"]);
        let result = detect_orphaned_sessions(&tasks, &runtime, 3).await;

        assert_eq!(result.alive, vec!["t1"]);
        assert_eq!(result.retried, vec!["t2"]);
        assert_eq!(result.failed, vec!["t3"]);
    }
}
