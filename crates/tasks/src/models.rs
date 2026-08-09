//! Core domain types.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::new_v4().simple()))
            }

            pub fn from_raw(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

id_newtype!(ProjectId, "proj");
id_newtype!(TaskId, "task");
id_newtype!(SessionId, "sess");
id_newtype!(SpecId, "spec");

/// A repo being tracked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub repo_owner: String,
    pub repo_name: String,
    pub added_at: DateTime<Utc>,
}

/// A unit of work, sourced from a GitHub issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub gh_issue_number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub gh_state: GhState,
    pub state: TaskState,
    pub priority: i32,
    /// Position in the human-curated queue, 1-based. `None` means unranked and
    /// sorts after every ranked task. Only ever written by explicit reorder
    /// requests — never by the GitHub poller.
    pub manual_rank: Option<i32>,
    /// Consecutive failed scout dispatches. Persisted so restarts can't reset
    /// the count: at `MAX_DISPATCH_ATTEMPTS` the dispatcher rejects the task
    /// instead of retrying it forever. Cleared when a scout produces a spec.
    /// Never written by the GitHub poller.
    pub dispatch_attempts: u32,
    pub ingested_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhState {
    Open,
    Closed,
}

impl GhState {
    pub fn as_str(&self) -> &'static str {
        match self {
            GhState::Open => "open",
            GhState::Closed => "closed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(GhState::Open),
            "closed" => Some(GhState::Closed),
            _ => None,
        }
    }
}

/// Where a task sits in our Diamond 1 pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Just ingested from GitHub.
    New,
    /// A Scout is actively exploring.
    Scouting,
    /// A Spec has been produced and awaits orchestrator review.
    SpecReady,
    /// Spec approved and sitting in the queue, waiting for Builder (out of scope this PR).
    Queued,
    /// Completed through the pipeline.
    Done,
    /// Rejected and won't be pursued.
    Rejected,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::New => "new",
            TaskState::Scouting => "scouting",
            TaskState::SpecReady => "spec_ready",
            TaskState::Queued => "queued",
            TaskState::Done => "done",
            TaskState::Rejected => "rejected",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "new" => Some(TaskState::New),
            "scouting" => Some(TaskState::Scouting),
            "spec_ready" => Some(TaskState::SpecReady),
            "queued" => Some(TaskState::Queued),
            "done" => Some(TaskState::Done),
            "rejected" => Some(TaskState::Rejected),
            _ => None,
        }
    }
}

/// A Scout run — one VM executing a throwaway implementation for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub task_id: TaskId,
    pub vm_id: Option<String>,
    pub branch: String,
    pub status: SessionStatus,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub exit_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    ScoutSucceeded,
    ScoutFailed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::ScoutSucceeded => "scout_succeeded",
            SessionStatus::ScoutFailed => "scout_failed",
            SessionStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(SessionStatus::Running),
            "scout_succeeded" => Some(SessionStatus::ScoutSucceeded),
            "scout_failed" => Some(SessionStatus::ScoutFailed),
            "cancelled" => Some(SessionStatus::Cancelled),
            _ => None,
        }
    }
}

/// The distilled artifact a Scout produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub id: SpecId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub content: String,
    pub complexity: Complexity,
    pub files_touched: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Simple,
    Medium,
    Complex,
}

impl Complexity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Complexity::Simple => "simple",
            Complexity::Medium => "medium",
            Complexity::Complex => "complex",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "simple" => Some(Complexity::Simple),
            "medium" => Some(Complexity::Medium),
            "complex" => Some(Complexity::Complex),
            _ => None,
        }
    }
}

/// An entry in the spec queue — the orchestrator's working set for judging
/// what should be implemented next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecQueueEntry {
    pub spec_id: SpecId,
    pub status: SpecQueueStatus,
    pub rank: Option<i32>,
    pub approved_at: Option<DateTime<Utc>>,
    pub feedback: Option<String>,
    pub blocking_dependencies: Vec<TaskId>,
}

/// A spec queue entry joined with the id of the task the spec was written for.
/// The queue listing needs the task id, and it lives on `specs`, not `spec_queue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecQueueItem {
    #[serde(flatten)]
    pub entry: SpecQueueEntry,
    pub task_id: TaskId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecQueueStatus {
    PendingReview,
    Approved,
    NeedsRevision,
    Blocked,
    Rejected,
}

impl SpecQueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SpecQueueStatus::PendingReview => "pending_review",
            SpecQueueStatus::Approved => "approved",
            SpecQueueStatus::NeedsRevision => "needs_revision",
            SpecQueueStatus::Blocked => "blocked",
            SpecQueueStatus::Rejected => "rejected",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending_review" => Some(SpecQueueStatus::PendingReview),
            "approved" => Some(SpecQueueStatus::Approved),
            "needs_revision" => Some(SpecQueueStatus::NeedsRevision),
            "blocked" => Some(SpecQueueStatus::Blocked),
            "rejected" => Some(SpecQueueStatus::Rejected),
            _ => None,
        }
    }
}

/// Global operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Play,
    Pause,
    Stop,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Play => "play",
            Mode::Pause => "pause",
            Mode::Stop => "stop",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "play" => Some(Mode::Play),
            "pause" => Some(Mode::Pause),
            "stop" => Some(Mode::Stop),
            _ => None,
        }
    }
}
