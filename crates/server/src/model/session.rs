//! Session model — spec Section 5.4.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Session status — spec Section 5.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Terminated,
}

impl SessionStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Terminated
        )
    }
}

/// A session — the unit of execution for a task.
///
/// Spec Section 5.4. One active session per task (spec Section 9.6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Session ID.
    pub id: String,
    /// The task this session is executing.
    pub task_id: String,
    /// Path to the sandboxed workspace.
    pub workspace_path: String,
    /// Git branch name.
    pub branch: String,
    /// Session status.
    pub status: SessionStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Session {
    pub fn new(
        id: impl Into<String>,
        task_id: impl Into<String>,
        workspace_path: impl Into<String>,
        branch: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            workspace_path: workspace_path.into(),
            branch: branch.into(),
            status: SessionStatus::Starting,
            started_at: Utc::now(),
            ended_at: None,
        }
    }

    pub fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
        if status.is_terminal() {
            self.ended_at = Some(Utc::now());
        }
    }
}
