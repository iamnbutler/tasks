//! Workspace model — spec Section 10.
//!
//! Tracks workspace lifecycle and enables cleanup policies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Workspace status for cleanup eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    /// Workspace is active (has a running session or recent activity).
    Active,
    /// Workspace is idle (no active session, eligible for stale check).
    Idle,
    /// Workspace is scheduled for cleanup.
    PendingCleanup,
    /// Workspace has been cleaned up.
    Cleaned,
}

/// A workspace — an isolated environment for a task's agent session.
///
/// Spec Section 10. A workspace includes:
/// - Container ID (the runtime environment)
/// - Git branch (the task's working branch)
/// - Last activity timestamp (for stale detection)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Workspace ID (matches task.workspace_id).
    pub id: String,
    /// The task that owns this workspace.
    pub task_id: String,
    /// Container ID for this workspace (if running).
    pub container_id: Option<String>,
    /// Git branch name.
    pub branch: Option<String>,
    /// Current status.
    pub status: WorkspaceStatus,
    /// When workspace was created.
    pub created_at: DateTime<Utc>,
    /// Last activity timestamp (for stale detection, spec §10.3).
    pub last_activity_at: DateTime<Utc>,
}

impl Workspace {
    /// Create a new workspace for a task.
    pub fn new(id: impl Into<String>, task_id: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            task_id: task_id.into(),
            container_id: None,
            branch: None,
            status: WorkspaceStatus::Active,
            created_at: now,
            last_activity_at: now,
        }
    }

    /// Update the last activity timestamp.
    pub fn touch(&mut self) {
        self.last_activity_at = Utc::now();
    }

    /// Mark workspace as idle.
    pub fn mark_idle(&mut self) {
        self.status = WorkspaceStatus::Idle;
    }

    /// Schedule workspace for cleanup.
    pub fn schedule_cleanup(&mut self) {
        self.status = WorkspaceStatus::PendingCleanup;
    }

    /// Mark workspace as cleaned.
    pub fn mark_cleaned(&mut self) {
        self.status = WorkspaceStatus::Cleaned;
        self.container_id = None;
    }

    /// Check if workspace is eligible for cleanup based on idle threshold.
    pub fn is_stale(&self, idle_threshold: chrono::Duration) -> bool {
        matches!(self.status, WorkspaceStatus::Idle | WorkspaceStatus::Active)
            && Utc::now().signed_duration_since(self.last_activity_at) > idle_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_workspace_is_active() {
        let ws = Workspace::new("ws-1", "task-1");
        assert_eq!(ws.status, WorkspaceStatus::Active);
        assert!(ws.container_id.is_none());
    }

    #[test]
    fn touch_updates_activity() {
        let mut ws = Workspace::new("ws-1", "task-1");
        let initial = ws.last_activity_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        ws.touch();
        assert!(ws.last_activity_at > initial);
    }

    #[test]
    fn is_stale_detects_idle_workspace() {
        let mut ws = Workspace::new("ws-1", "task-1");
        ws.last_activity_at = Utc::now() - chrono::Duration::days(10);
        ws.status = WorkspaceStatus::Idle;

        assert!(ws.is_stale(chrono::Duration::days(7)));
        assert!(!ws.is_stale(chrono::Duration::days(14)));
    }

    #[test]
    fn cleaned_workspace_not_stale() {
        let mut ws = Workspace::new("ws-1", "task-1");
        ws.last_activity_at = Utc::now() - chrono::Duration::days(10);
        ws.mark_cleaned();

        assert!(!ws.is_stale(chrono::Duration::days(7)));
    }
}
