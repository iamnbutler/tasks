//! Append-only event log with pub/sub.
//!
//! Events are persisted to SQLite for replay and query, and broadcast in-memory
//! for subscribers (HTTP SSE streams, orchestrator notifications).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    GhState, Mode, ProjectId, SessionId, SessionStatus, SpecId, SpecQueueStatus, TaskId, TaskState,
};

/// A timestamped, sequenced record. `seq` is assigned by the store on append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub timestamp: DateTime<Utc>,
    pub payload: EventPayload,
}

/// Discriminated union of everything that can happen in the system.
///
/// Keep variants flat and identifier-only where possible — consumers pull detail
/// via store lookups. This keeps events small and easy to serialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    ProjectAdded {
        project_id: ProjectId,
    },
    TaskIngested {
        task_id: TaskId,
        project_id: ProjectId,
    },
    TaskStateChanged {
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
    },
    /// The snapshot of GitHub's open/closed flag on a task changed. Emitted by
    /// the poller — notably when an issue disappears from a repository's open
    /// set, which is the only signal GitHub gives us that it was closed.
    TaskGhStateChanged {
        task_id: TaskId,
        gh_state: GhState,
    },
    SessionStarted {
        session_id: SessionId,
        task_id: TaskId,
    },
    SessionCompleted {
        session_id: SessionId,
        task_id: TaskId,
        status: SessionStatus,
    },
    SpecCreated {
        spec_id: SpecId,
        task_id: TaskId,
        session_id: SessionId,
    },
    SpecQueueStatusChanged {
        spec_id: SpecId,
        from: Option<SpecQueueStatus>,
        to: SpecQueueStatus,
    },
    /// The human-curated task queue was reordered. `task_ids` is the new order,
    /// front to back; tasks not listed were left unranked.
    QueueReordered {
        task_ids: Vec<TaskId>,
    },
    /// The spec queue was reordered. Same semantics as [`Self::QueueReordered`].
    SpecQueueReordered {
        spec_ids: Vec<SpecId>,
    },
    ModeChanged {
        from: Mode,
        to: Mode,
    },
    /// Free-form breadcrumb from the orchestrator (or other subsystems) — used
    /// for humans watching the event stream. Not consumed programmatically.
    Note {
        source: String,
        message: String,
    },
}
