//! Merge queue entry model — spec Section 5.5, 7.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Merge queue entry status — spec Section 5.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    Pending,
    Approved,
    Rejected,
    Merged,
    Conflict,
}

/// Type of merge conflict — spec Section 7.4.
///
/// Distinguishes between conflicts that can be resolved mechanically
/// (by the orchestrator or automated tooling) and those requiring
/// human or agent intervention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictType {
    /// Branch is behind the base branch but no file conflicts exist.
    /// Can be resolved with a simple rebase or merge from base.
    NeedsRebase,
    /// Git merge conflicts exist but appear to be in auto-generated files
    /// (lockfiles, build artifacts, etc.) that can be regenerated.
    TrivialMerge,
    /// Git merge conflicts in source code that may be resolvable by
    /// re-engaging the implementor agent.
    SourceConflict,
    /// Complex conflicts involving significant code changes that
    /// likely require human guidance.
    ComplexConflict,
    /// GitHub has not yet computed mergeability (try again later).
    Unknown,
}

impl ConflictType {
    /// Whether this conflict type can be resolved mechanically without
    /// human intervention (spec Section 7.4).
    pub fn is_mechanical(&self) -> bool {
        matches!(self, Self::NeedsRebase | Self::TrivialMerge)
    }

    /// Whether this conflict should be surfaced to a human in Pause mode.
    pub fn needs_human_guidance(&self) -> bool {
        matches!(self, Self::ComplexConflict)
    }
}

/// Information about a detected merge conflict — spec Section 7.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    /// The type of conflict detected.
    pub conflict_type: ConflictType,
    /// Files involved in the conflict (if known).
    pub conflicting_files: Vec<String>,
    /// Human-readable description of the conflict.
    pub description: String,
    /// When the conflict was detected.
    pub detected_at: DateTime<Utc>,
}

/// A merge queue entry — a PR waiting to be merged.
///
/// The merge queue is a list of PRs, ordered by when they were queued.
/// A `task_id` links back to the task that produced the PR, but the
/// queue itself is PR-centric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeQueueEntry {
    /// Queue entry ID.
    pub id: String,
    /// The task that produced this PR.
    pub task_id: String,
    /// GitHub PR URL.
    pub pr_url: String,
    pub status: MergeStatus,
    pub queued_at: DateTime<Utc>,
    /// Conflict information, populated when status is Conflict (spec §7.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_info: Option<ConflictInfo>,
}

impl MergeQueueEntry {
    pub fn new(id: impl Into<String>, task_id: impl Into<String>, pr_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            pr_url: pr_url.into(),
            status: MergeStatus::Pending,
            queued_at: Utc::now(),
            conflict_info: None,
        }
    }

    /// Set the entry to conflict status with detailed information (spec §7.4).
    pub fn set_conflict(&mut self, info: ConflictInfo) {
        self.status = MergeStatus::Conflict;
        self.conflict_info = Some(info);
    }

    /// Clear conflict status and return to pending (after resolution).
    pub fn clear_conflict(&mut self) {
        self.status = MergeStatus::Pending;
        self.conflict_info = None;
    }

    /// Check if this entry has a mechanical conflict that can be auto-resolved.
    pub fn has_mechanical_conflict(&self) -> bool {
        self.conflict_info
            .as_ref()
            .is_some_and(|info| info.conflict_type.is_mechanical())
    }

    /// Check if this conflict needs human guidance.
    pub fn needs_human_guidance(&self) -> bool {
        self.conflict_info
            .as_ref()
            .is_some_and(|info| info.conflict_type.needs_human_guidance())
    }
}
