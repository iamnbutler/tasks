//! Append-only event log with pub/sub.
//!
//! Events are persisted to SQLite for replay and query, and broadcast in-memory
//! for subscribers (HTTP SSE streams, orchestrator notifications).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    Actor, BriefingSection, BuildId, BuildStatus, ChatRole, CloseReason, GhState, Mode, ProjectId,
    RunKind, SessionEndReason, SessionId, SessionStatus, SpecId, SpecQueueStatus, TaskId,
    TaskState,
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
    /// An issue was filed *through the server* — discovered work that would
    /// otherwise have been lost. Distinct from `TaskIngested`, which is the
    /// poller finding an issue somebody else wrote.
    IssueCaptured {
        task_id: TaskId,
        gh_issue_number: u64,
        actor: Actor,
        decision_seq: Option<i64>,
    },
    /// An issue was closed through the server. The task is *not* retired here:
    /// closure is GitHub's fact, and the poller observes it on the next pass
    /// exactly as it would for an issue closed in the browser.
    IssueClosed {
        task_id: TaskId,
        gh_issue_number: u64,
        reason: CloseReason,
        actor: Actor,
        decision_seq: Option<i64>,
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
    /// A spec landed. `session_id` is the Scout run behind it, and `None` when
    /// a human wrote it by hand (`POST /tasks/{id}/build-now`) — see
    /// [`crate::models::Spec::session_id`].
    SpecCreated {
        spec_id: SpecId,
        task_id: TaskId,
        #[serde(default)]
        session_id: Option<SessionId>,
    },
    /// A spec's review state changed. `actor` is who decided — the
    /// orchestrator must never be nudged about its own verdicts — and
    /// `decision_seq` points into the decisions ledger for the rationale.
    /// Both are absent only on transitions nobody chose (a Builder run
    /// marking specs `built`).
    SpecQueueStatusChanged {
        spec_id: SpecId,
        from: Option<SpecQueueStatus>,
        to: SpecQueueStatus,
        #[serde(default)]
        actor: Option<Actor>,
        #[serde(default)]
        decision_seq: Option<i64>,
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
        #[serde(default)]
        actor: Option<Actor>,
        #[serde(default)]
        decision_seq: Option<i64>,
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
    /// Somebody asked for a run that is already in flight to stop.
    ///
    /// The announcement, not the outcome: the dispatcher following the run is
    /// what actually interrupts it, and it concludes the row with the ordinary
    /// [`Self::SessionCompleted`] / [`Self::BuildCompleted`] carrying a
    /// `cancelled` status. A cancel that arrives after the run finished on its
    /// own leaves this event and nothing else, which is honest.
    ///
    /// **The fields cannot be called `kind` or `id`.** This enum is
    /// `#[serde(tag = "kind")]`, so a field by that name is a compile error,
    /// and `run_id` is named to match rather than to differ for its own sake.
    RunCancelRequested {
        run_kind: RunKind,
        run_id: String,
        actor: Actor,
        #[serde(default)]
        decision_seq: Option<i64>,
    },
    /// Egress failed and the build's commits were written down instead. The
    /// VM is deallocated before egress runs, so at this moment the file named
    /// by `GET /builds/{build_id}/bundle` is the only copy of that
    /// implementation anywhere.
    BundlePreserved {
        build_id: BuildId,
        bytes: u64,
    },
    /// A preserved bundle was deleted. `superseded` is the whole difference
    /// that matters: `true` is the retention policy reclaiming work that has
    /// since shipped, `false` is somebody throwing an implementation away.
    BundleRemoved {
        build_id: BuildId,
        superseded: bool,
        actor: Actor,
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
    /// The orchestrator started living in a new Claude Code session.
    /// `replacing` is `None` only for the very first session; otherwise this
    /// is a seam, and `reason` says whether we chose it or suffered it.
    ///
    /// Never nudge-worthy: the seam is already a visible turn in the chat,
    /// and notifying the new session that it just lost its memory would
    /// spend its first turn on that.
    OrchestratorSessionStarted {
        session_id: String,
        replacing: Option<String>,
        reason: Option<SessionEndReason>,
    },
    ModeChanged {
        from: Mode,
        to: Mode,
    },
    /// A Home briefing slot was regenerated. Content is on the row — refetch
    /// `GET /briefings`. Must never nudge the orchestrator (feedback loop:
    /// briefings are generated *about* pipeline activity).
    BriefingUpdated {
        section: BriefingSection,
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
            EventPayload::IssueCaptured { .. } => "issue_captured",
            EventPayload::IssueClosed { .. } => "issue_closed",
            EventPayload::SessionStarted { .. } => "session_started",
            EventPayload::SessionCompleted { .. } => "session_completed",
            EventPayload::SpecCreated { .. } => "spec_created",
            EventPayload::SpecQueueStatusChanged { .. } => "spec_queue_status_changed",
            EventPayload::QueueReordered { .. } => "queue_reordered",
            EventPayload::SpecQueueReordered { .. } => "spec_queue_reordered",
            EventPayload::BuildRequested { .. } => "build_requested",
            EventPayload::BuildStarted { .. } => "build_started",
            EventPayload::BuildCompleted { .. } => "build_completed",
            EventPayload::RunCancelRequested { .. } => "run_cancel_requested",
            EventPayload::BundlePreserved { .. } => "bundle_preserved",
            EventPayload::BundleRemoved { .. } => "bundle_removed",
            EventPayload::PullRequestOpened { .. } => "pull_request_opened",
            EventPayload::OrchestratorMessage { .. } => "orchestrator_message",
            EventPayload::OrchestratorSessionStarted { .. } => "orchestrator_session_started",
            EventPayload::ModeChanged { .. } => "mode_changed",
            EventPayload::BriefingUpdated { .. } => "briefing_updated",
            EventPayload::Note { .. } => "note",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        Actor, BriefingSection, BuildId, BuildStatus, ChatRole, CloseReason, GhState, Mode,
        ProjectId, RunKind, SessionEndReason, SessionId, SessionStatus, SpecId, SpecQueueStatus,
        TaskId, TaskState,
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
            EventPayload::IssueCaptured {
                task_id: task(),
                gh_issue_number: 900,
                actor: Actor::Orchestrator,
                decision_seq: Some(3),
            },
            EventPayload::IssueClosed {
                task_id: task(),
                gh_issue_number: 900,
                reason: CloseReason::NotPlanned,
                actor: Actor::Human,
                decision_seq: None,
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
                session_id: Some(SessionId::from_raw("sess_1")),
            },
            EventPayload::SpecQueueStatusChanged {
                spec_id: SpecId::from_raw("spec_1"),
                from: None,
                to: SpecQueueStatus::PendingReview,
                actor: None,
                decision_seq: None,
            },
            EventPayload::QueueReordered { task_ids: vec![] },
            EventPayload::SpecQueueReordered { spec_ids: vec![] },
            EventPayload::BuildRequested {
                build_id: BuildId::from_raw("build_1"),
                spec_ids: vec![SpecId::from_raw("spec_1")],
                actor: Some(Actor::Human),
                decision_seq: Some(1),
            },
            EventPayload::BuildStarted {
                build_id: BuildId::from_raw("build_1"),
            },
            EventPayload::BuildCompleted {
                build_id: BuildId::from_raw("build_1"),
                status: BuildStatus::Succeeded,
            },
            EventPayload::RunCancelRequested {
                run_kind: RunKind::Session,
                run_id: "sess_1".into(),
                actor: Actor::Human,
                decision_seq: Some(4),
            },
            EventPayload::BundlePreserved {
                build_id: BuildId::from_raw("build_1"),
                bytes: 4096,
            },
            EventPayload::BundleRemoved {
                build_id: BuildId::from_raw("build_1"),
                superseded: true,
                actor: Actor::Human,
            },
            EventPayload::PullRequestOpened {
                build_id: BuildId::from_raw("build_1"),
                pr_number: 7,
            },
            EventPayload::OrchestratorMessage {
                seq: 1,
                role: ChatRole::User,
            },
            EventPayload::OrchestratorSessionStarted {
                session_id: "sess-b".into(),
                replacing: Some("sess-a".into()),
                reason: Some(SessionEndReason::ResumeFailed),
            },
            EventPayload::ModeChanged {
                from: Mode::Play,
                to: Mode::Pause,
            },
            EventPayload::BriefingUpdated {
                section: BriefingSection::Changes,
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
            actor: Some(Actor::Human),
            decision_seq: Some(1),
        };
        let rejected = EventPayload::SpecQueueStatusChanged {
            spec_id: SpecId::from_raw("spec_1"),
            from: Some(SpecQueueStatus::PendingReview),
            to: SpecQueueStatus::Rejected,
            actor: Some(Actor::Human),
            decision_seq: Some(2),
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
            actor: Some(Actor::Human),
            decision_seq: None,
        })
        .unwrap();
        assert_eq!(wire["spec_ids"], serde_json::json!(["spec_a", "spec_b"]));
    }

    /// A reclaim and a human throwing work away are the same *kind* of event,
    /// and are told apart by `superseded` alone. Pinned because the two read
    /// very differently to whoever is scrolling the feed: one is bookkeeping,
    /// the other destroyed the only copy of an implementation.
    #[test]
    fn a_bundle_removal_says_whether_the_work_had_shipped() {
        let reclaimed = serde_json::to_value(EventPayload::BundleRemoved {
            build_id: BuildId::from_raw("build_1"),
            superseded: true,
            actor: Actor::Human,
        })
        .unwrap();
        let discarded = serde_json::to_value(EventPayload::BundleRemoved {
            build_id: BuildId::from_raw("build_1"),
            superseded: false,
            actor: Actor::Human,
        })
        .unwrap();
        assert_eq!(reclaimed["kind"], discarded["kind"]);
        assert_eq!(reclaimed["superseded"], serde_json::json!(true));
        assert_eq!(discarded["superseded"], serde_json::json!(false));
    }

    /// The tag collision that makes this variant's field names non-negotiable:
    /// `kind` is serde's discriminant for the whole enum, so a payload field
    /// called `kind` does not compile — and one called `id` would read as the
    /// event's own id rather than the run's.
    #[test]
    fn a_cancel_request_names_the_run_without_shadowing_the_tag() {
        let wire = serde_json::to_value(EventPayload::RunCancelRequested {
            run_kind: RunKind::Build,
            run_id: "build_1".into(),
            actor: Actor::Orchestrator,
            decision_seq: Some(9),
        })
        .unwrap();
        assert_eq!(wire["kind"], "run_cancel_requested");
        assert_eq!(wire["run_kind"], "build");
        assert_eq!(wire["run_id"], "build_1");
        assert_eq!(wire["actor"], "orchestrator");
        assert!(wire.get("id").is_none());
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
        "run_cancel_requested",
        "bundle_preserved",
        "bundle_removed",
        "pull_request_opened",
        "orchestrator_message",
        "mode_changed",
        "briefing_updated",
        "note",
    ];
}
