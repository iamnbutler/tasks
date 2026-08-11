//! Append-only event log with pub/sub.
//!
//! Events are persisted to SQLite for replay and query, and broadcast in-memory
//! for subscribers (HTTP SSE streams, orchestrator notifications).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    BuildId, BuildStatus, ChatRole, GhState, Mode, ProjectId, SessionId, SessionStatus, SpecId,
    SpecQueueStatus, TaskId, TaskState,
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
    /// A Builder run was requested over a set of approved specs.
    BuildRequested {
        build_id: BuildId,
        spec_ids: Vec<SpecId>,
    },
    /// The serial build loop claimed the build; a Builder VM is running it.
    BuildStarted {
        build_id: BuildId,
    },
    /// The build reached a terminal status. Detail (branch, PR, exit reason)
    /// is on the build row — refetch it.
    BuildCompleted {
        build_id: BuildId,
        status: BuildStatus,
    },
    /// The server pushed the branch and opened the pull request. `pr_number`
    /// is an identifier: the PR's state is GitHub's, queried, never stored.
    PullRequestOpened {
        build_id: BuildId,
        pr_number: u64,
    },
    /// A turn was appended to the orchestrator conversation. Content is on
    /// the message row — refetch `/orchestrator/messages?since=`.
    OrchestratorMessage {
        seq: i64,
        role: ChatRole,
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

impl EventPayload {
    /// The wire `kind` of this payload — the same string serde's
    /// `tag = "kind"` emits. Exhaustive on purpose: clients count events by
    /// matching these strings (the app's velocity fold, docs/clients.md), so
    /// adding or renaming a variant must not compile until this list — and
    /// therefore the conversation about who consumes the string — is updated.
    pub fn kind(&self) -> &'static str {
        match self {
            EventPayload::ProjectAdded { .. } => "project_added",
            EventPayload::TaskIngested { .. } => "task_ingested",
            EventPayload::TaskStateChanged { .. } => "task_state_changed",
            EventPayload::TaskGhStateChanged { .. } => "task_gh_state_changed",
            EventPayload::SessionStarted { .. } => "session_started",
            EventPayload::SessionCompleted { .. } => "session_completed",
            EventPayload::SpecCreated { .. } => "spec_created",
            EventPayload::SpecQueueStatusChanged { .. } => "spec_queue_status_changed",
            EventPayload::QueueReordered { .. } => "queue_reordered",
            EventPayload::SpecQueueReordered { .. } => "spec_queue_reordered",
            EventPayload::BuildRequested { .. } => "build_requested",
            EventPayload::BuildStarted { .. } => "build_started",
            EventPayload::BuildCompleted { .. } => "build_completed",
            EventPayload::PullRequestOpened { .. } => "pull_request_opened",
            EventPayload::OrchestratorMessage { .. } => "orchestrator_message",
            EventPayload::ModeChanged { .. } => "mode_changed",
            EventPayload::Note { .. } => "note",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        BuildId, BuildStatus, ChatRole, GhState, Mode, ProjectId, SessionId, SessionStatus, SpecId,
        SpecQueueStatus, TaskId, TaskState,
    };

    fn task() -> TaskId {
        TaskId::from_raw("task_1")
    }

    /// Every declared kind matches what serde actually puts on the wire.
    /// This is the forcing function for clients that count by string.
    #[test]
    fn declared_kinds_match_the_wire() {
        let samples = vec![
            EventPayload::ProjectAdded {
                project_id: ProjectId::from_raw("proj_1"),
            },
            EventPayload::TaskIngested {
                task_id: task(),
                project_id: ProjectId::from_raw("proj_1"),
            },
            EventPayload::TaskStateChanged {
                task_id: task(),
                from: TaskState::Backlog,
                to: TaskState::Queued,
            },
            EventPayload::TaskGhStateChanged {
                task_id: task(),
                gh_state: GhState::Closed,
            },
            EventPayload::SessionStarted {
                session_id: SessionId::from_raw("sess_1"),
                task_id: task(),
            },
            EventPayload::SessionCompleted {
                session_id: SessionId::from_raw("sess_1"),
                task_id: task(),
                status: SessionStatus::ScoutSucceeded,
            },
            EventPayload::SpecCreated {
                spec_id: SpecId::from_raw("spec_1"),
                task_id: task(),
                session_id: SessionId::from_raw("sess_1"),
            },
            EventPayload::SpecQueueStatusChanged {
                spec_id: SpecId::from_raw("spec_1"),
                from: None,
                to: SpecQueueStatus::PendingReview,
            },
            EventPayload::QueueReordered { task_ids: vec![] },
            EventPayload::SpecQueueReordered { spec_ids: vec![] },
            EventPayload::BuildRequested {
                build_id: BuildId::from_raw("build_1"),
                spec_ids: vec![SpecId::from_raw("spec_1")],
            },
            EventPayload::BuildStarted {
                build_id: BuildId::from_raw("build_1"),
            },
            EventPayload::BuildCompleted {
                build_id: BuildId::from_raw("build_1"),
                status: BuildStatus::Succeeded,
            },
            EventPayload::PullRequestOpened {
                build_id: BuildId::from_raw("build_1"),
                pr_number: 7,
            },
            EventPayload::OrchestratorMessage {
                seq: 1,
                role: ChatRole::User,
            },
            EventPayload::ModeChanged {
                from: Mode::Play,
                to: Mode::Pause,
            },
            EventPayload::Note {
                source: "test".into(),
                message: "hi".into(),
            },
        ];
        for payload in samples {
            let wire: serde_json::Value = serde_json::to_value(&payload).unwrap();
            assert_eq!(
                wire["kind"],
                payload.kind(),
                "declared kind diverges from the wire for {payload:?}"
            );
        }
    }

    /// The five kinds the app's velocity fold counts, pinned by name — and
    /// the subtlety that an approval and a rejection are the SAME kind,
    /// distinguished only by `to`.
    #[test]
    fn velocity_vocabulary_is_the_client_contract() {
        for kind in [
            "task_ingested",
            "spec_created",
            "spec_queue_status_changed",
            "build_completed",
            "pull_request_opened",
        ] {
            // The vocabulary exists: at least one variant declares it.
            // (A rename upstream breaks `declared_kinds_match_the_wire`
            // first; this pins the client-facing five explicitly.)
            assert!(
                KINDS.contains(&kind),
                "velocity counts `{kind}` but no variant declares it"
            );
        }
        let approved = EventPayload::SpecQueueStatusChanged {
            spec_id: SpecId::from_raw("spec_1"),
            from: Some(SpecQueueStatus::PendingReview),
            to: SpecQueueStatus::Approved,
        };
        let rejected = EventPayload::SpecQueueStatusChanged {
            spec_id: SpecId::from_raw("spec_1"),
            from: Some(SpecQueueStatus::PendingReview),
            to: SpecQueueStatus::Rejected,
        };
        let approved_wire = serde_json::to_value(&approved).unwrap();
        let rejected_wire = serde_json::to_value(&rejected).unwrap();
        assert_eq!(approved_wire["kind"], rejected_wire["kind"]);
        assert_eq!(approved_wire["to"], "approved");
        assert_eq!(rejected_wire["to"], "rejected");
    }

    /// `build_requested` carries the spec ids — the only place the wire says
    /// which work a build serves without fetching `/builds/{id}`.
    #[test]
    fn build_requested_carries_spec_ids() {
        let wire = serde_json::to_value(EventPayload::BuildRequested {
            build_id: BuildId::from_raw("build_1"),
            spec_ids: vec![SpecId::from_raw("spec_a"), SpecId::from_raw("spec_b")],
        })
        .unwrap();
        assert_eq!(wire["spec_ids"], serde_json::json!(["spec_a", "spec_b"]));
    }

    /// Every kind string, for the vocabulary test.
    const KINDS: &[&str] = &[
        "project_added",
        "task_ingested",
        "task_state_changed",
        "task_gh_state_changed",
        "session_started",
        "session_completed",
        "spec_created",
        "spec_queue_status_changed",
        "queue_reordered",
        "spec_queue_reordered",
        "build_requested",
        "build_started",
        "build_completed",
        "pull_request_opened",
        "orchestrator_message",
        "mode_changed",
        "note",
    ];
}
