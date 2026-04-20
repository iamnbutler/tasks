//! Spec artifact and Spec Queue — Double Diamond Diamond 1 output.
//!
//! See `spec/double-diamond.md` §3 for the full data model.
//!
//! A [`Spec`] is the artifact a Scout produces: markdown distilled from a throwaway
//! implementation, capturing the implementation approach, discovered pitfalls, and
//! dependencies. Specs are reviewed by the orchestrator, queued, and consumed by a
//! single Builder per project.
//!
//! This module defines the domain types only. Persistence, queue ordering, and
//! dispatch integration are handled in later phases (see `spec/double-diamond.md` §10).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a Spec — Double Diamond §3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecStatus {
    /// Scout just submitted; orchestrator hasn't reviewed yet.
    PendingReview,
    /// Approved by orchestrator and available to the Builder pool.
    Approved,
    /// Orchestrator sent feedback; the Scout task has been re-dispatched.
    NeedsRevision,
    /// At least one dependency Spec has not yet merged.
    Blocked,
    /// A Builder has claimed this Spec.
    Consumed,
    /// Replaced by a later revision of the same issue.
    Superseded,
    /// Issue withdrawn, or re-exploration from scratch requested.
    Rejected,
}

impl SpecStatus {
    /// Whether a Spec in this state is eligible for the Builder to claim.
    pub fn is_buildable(&self) -> bool {
        matches!(self, SpecStatus::Approved)
    }

    /// Whether this is a terminal state (no further status transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, SpecStatus::Consumed | SpecStatus::Superseded | SpecStatus::Rejected)
    }
}

/// Coarse complexity hint from the Scout — Double Diamond §3.2.
///
/// Used by Spec Queue prioritization (§5.2) and by the orchestrator to decide
/// whether review needs extra scrutiny. Not a substitute for actual diff size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Simple,
    Medium,
    Complex,
}

/// A Spec — the artifact Diamond 1 produces.
///
/// Spec content is structured markdown (§4.3). This type does not parse the markdown;
/// it carries metadata extracted from it by the Scout output parser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// Internal Spec ID.
    pub id: String,
    /// The umbrella task representing the user-facing issue.
    pub issue_task_id: String,
    /// The Scout task that produced this Spec.
    pub scout_task_id: String,
    /// Full Spec markdown, structured per Double Diamond §4.3.
    pub content: String,
    /// Scout's complexity estimate.
    pub complexity: Complexity,
    /// Issue task IDs that must merge before a Builder claims this Spec.
    pub dependencies: Vec<String>,
    /// File paths the Scout touched during throwaway implementation.
    /// Used for staleness detection (§5.3) and queue prioritization (§5.2).
    pub files_touched: Vec<String>,
    pub status: SpecStatus,
    /// Bumped on re-exploration. Starts at 1.
    pub revision: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Spec {
    /// Create a new Spec in `PendingReview` at revision 1.
    pub fn new(
        id: impl Into<String>,
        issue_task_id: impl Into<String>,
        scout_task_id: impl Into<String>,
        content: impl Into<String>,
        complexity: Complexity,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            issue_task_id: issue_task_id.into(),
            scout_task_id: scout_task_id.into(),
            content: content.into(),
            complexity,
            dependencies: Vec::new(),
            files_touched: Vec::new(),
            status: SpecStatus::PendingReview,
            revision: 1,
            created_at: now,
            updated_at: now,
        }
    }

    /// Transition to a new status, updating `updated_at`.
    ///
    /// Does not enforce transitions — orchestrator logic does. This mirrors the
    /// soft-validation pattern used by `Task::set_state`.
    pub fn set_status(&mut self, status: SpecStatus) {
        self.status = status;
        self.updated_at = Utc::now();
    }
}

/// Discriminator on `Task` — Double Diamond §3.1.
///
/// Added so that a single `Task` row can represent any of the three roles in the
/// Double Diamond, reusing the existing state machine, retry machinery, and
/// dispatch plumbing. The `Implement` variant preserves the pre-Double-Diamond
/// behavior for legacy and fast-path (§8) tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    /// Legacy single-phase "implement the issue" task, and the fast path (§8).
    Implement,
    /// Diamond 1 exploration. Produces a Spec.
    Scout {
        /// The umbrella issue task this Scout is exploring.
        issue_task_id: String,
        /// 1-based attempt number. Increments on re-exploration.
        attempt: u32,
    },
    /// Diamond 2 implementation. Consumes a Spec, produces a PR.
    Builder {
        /// The Spec being implemented.
        spec_id: String,
    },
}

impl Default for TaskKind {
    fn default() -> Self {
        TaskKind::Implement
    }
}

impl TaskKind {
    pub fn is_scout(&self) -> bool {
        matches!(self, TaskKind::Scout { .. })
    }

    pub fn is_builder(&self) -> bool {
        matches!(self, TaskKind::Builder { .. })
    }

    pub fn is_implement(&self) -> bool {
        matches!(self, TaskKind::Implement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_status_buildable_only_approved() {
        assert!(SpecStatus::Approved.is_buildable());
        assert!(!SpecStatus::PendingReview.is_buildable());
        assert!(!SpecStatus::NeedsRevision.is_buildable());
        assert!(!SpecStatus::Blocked.is_buildable());
        assert!(!SpecStatus::Consumed.is_buildable());
        assert!(!SpecStatus::Superseded.is_buildable());
        assert!(!SpecStatus::Rejected.is_buildable());
    }

    #[test]
    fn spec_status_terminal() {
        assert!(SpecStatus::Consumed.is_terminal());
        assert!(SpecStatus::Superseded.is_terminal());
        assert!(SpecStatus::Rejected.is_terminal());
        assert!(!SpecStatus::Approved.is_terminal());
        assert!(!SpecStatus::PendingReview.is_terminal());
    }

    #[test]
    fn spec_new_defaults() {
        let s = Spec::new("spec-1", "task-1", "scout-1", "# spec", Complexity::Medium);
        assert_eq!(s.id, "spec-1");
        assert_eq!(s.status, SpecStatus::PendingReview);
        assert_eq!(s.revision, 1);
        assert!(s.dependencies.is_empty());
        assert!(s.files_touched.is_empty());
    }

    #[test]
    fn spec_set_status_updates_timestamp() {
        let mut s = Spec::new("s", "t", "sc", "c", Complexity::Simple);
        let before = s.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.set_status(SpecStatus::Approved);
        assert_eq!(s.status, SpecStatus::Approved);
        assert!(s.updated_at > before);
    }

    #[test]
    fn task_kind_default_is_implement() {
        assert!(TaskKind::default().is_implement());
    }

    #[test]
    fn task_kind_discriminators() {
        let scout = TaskKind::Scout { issue_task_id: "t".into(), attempt: 1 };
        let builder = TaskKind::Builder { spec_id: "s".into() };
        let implement = TaskKind::Implement;

        assert!(scout.is_scout() && !scout.is_builder() && !scout.is_implement());
        assert!(builder.is_builder() && !builder.is_scout() && !builder.is_implement());
        assert!(implement.is_implement() && !implement.is_scout() && !implement.is_builder());
    }

    #[test]
    fn task_kind_serde_round_trip() {
        let cases = [
            TaskKind::Implement,
            TaskKind::Scout { issue_task_id: "task-1".into(), attempt: 2 },
            TaskKind::Builder { spec_id: "spec-1".into() },
        ];
        for k in cases {
            let json = serde_json::to_string(&k).unwrap();
            let back: TaskKind = serde_json::from_str(&json).unwrap();
            assert_eq!(k, back);
        }
    }

    #[test]
    fn spec_serde_round_trip() {
        let mut s = Spec::new("s", "t", "sc", "content", Complexity::Complex);
        s.dependencies = vec!["t-2".into(), "t-3".into()];
        s.files_touched = vec!["crates/models/src/lib.rs".into()];
        let json = serde_json::to_string(&s).unwrap();
        let back: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, s.id);
        assert_eq!(back.dependencies, s.dependencies);
        assert_eq!(back.files_touched, s.files_touched);
        assert_eq!(back.complexity, Complexity::Complex);
    }

    #[test]
    fn complexity_serde_snake_case() {
        assert_eq!(serde_json::to_string(&Complexity::Simple).unwrap(), "\"simple\"");
        assert_eq!(serde_json::to_string(&Complexity::Medium).unwrap(), "\"medium\"");
        assert_eq!(serde_json::to_string(&Complexity::Complex).unwrap(), "\"complex\"");
    }
}
