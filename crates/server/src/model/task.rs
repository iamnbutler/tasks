//! Task model — spec Section 5.1, 5.2, 5.3.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Origin reference for a task (spec Section 5.1 `source` field).
///
/// A task may originate from a GitHub issue, a GitHub PR, or be created
/// internally by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Implementation complete, in merge queue.
    AwaitingMerge,
    /// Merge conflict needs resolution.
    Conflict,
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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
            created_at: now,
            updated_at: now,
        }
    }

    /// Transition to a new state, updating the timestamp.
    pub fn set_state(&mut self, state: TaskState) {
        self.state = state;
        self.updated_at = Utc::now();
    }
}
