//! Merge queue entry model — spec Section 5.5, 7.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Merge queue entry status — spec Section 5.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    Pending,
    Approved,
    /// Actively being merged — GitHub API call in progress.
    Merging,
    Rejected,
    Merged,
    Conflict,
    /// Changes requested by orchestrator or human — PR needs work before re-evaluation.
    /// Unlike Rejected, the entry stays in the queue and the task gets priority dispatch.
    ChangesRequested,
}

impl MergeStatus {
    /// Whether this is a terminal state (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Merged | Self::Rejected)
    }
}

/// Type of merge conflict — spec Section 7.4.
///
/// The orchestrator uses this to decide resolution strategy:
/// - Mechanical conflicts (NeedsRebase, TrivialMerge) can be resolved automatically
/// - Source conflicts require agent re-engagement
/// - Complex conflicts may need human guidance
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictType {
    /// Branch is behind base branch — needs rebase (mechanical).
    NeedsRebase,
    /// Conflicts in generated/lock files only — can auto-resolve (mechanical).
    TrivialMerge,
    /// Conflicts in source code — needs agent re-engagement.
    SourceConflict,
    /// Extensive conflicts across many files — may need human guidance.
    ComplexConflict,
    /// GitHub hasn't computed mergeability yet.
    Unknown,
}

impl ConflictType {
    /// Returns true if this conflict type can be resolved mechanically.
    pub fn is_mechanical(&self) -> bool {
        matches!(self, ConflictType::NeedsRebase | ConflictType::TrivialMerge)
    }
}

/// Detailed information about a merge conflict — spec Section 7.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    /// The type of conflict detected.
    pub conflict_type: ConflictType,
    /// Files involved in the conflict, if known.
    pub conflicting_files: Vec<String>,
    /// Human-readable description of the conflict.
    pub description: String,
    /// When the conflict was detected.
    pub detected_at: DateTime<Utc>,
}

impl ConflictInfo {
    pub fn new(conflict_type: ConflictType, description: impl Into<String>) -> Self {
        Self {
            conflict_type,
            conflicting_files: Vec::new(),
            description: description.into(),
            detected_at: Utc::now(),
        }
    }

    pub fn with_files(mut self, files: Vec<String>) -> Self {
        self.conflicting_files = files;
        self
    }
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
    /// Conflict details when status is Conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_info: Option<ConflictInfo>,
    /// Feedback provided when changes were requested.
    /// Set when status is ChangesRequested to guide the agent on what to fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_requested_feedback: Option<String>,
    /// Current head commit SHA of the PR branch.
    /// Used to detect new commits for re-evaluation (spec Section 7.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Position in merge queue (1-indexed).
    /// Only set for Approved/Merging entries to show merge order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    /// Timestamp when the entry transitioned to a terminal state (Merged/Rejected).
    /// Used by cleanup to implement a cooldown period before removal, preventing
    /// race conditions where GitHub's merged state hasn't propagated yet (issue #438).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Whether GitHub's mergeable status is Unknown (computation pending).
    /// When true, the entry should not be approved or merged until GitHub
    /// resolves the mergeability check (issue #503).
    #[serde(default)]
    pub mergeable_unknown: bool,
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
            changes_requested_feedback: None,
            head_sha: None,
            queue_position: None,
            completed_at: None,
            mergeable_unknown: false,
        }
    }

    /// Set the head SHA for this entry (builder pattern).
    pub fn with_head_sha(mut self, sha: impl Into<String>) -> Self {
        self.head_sha = Some(sha.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_status_is_terminal() {
        assert!(MergeStatus::Merged.is_terminal());
        assert!(MergeStatus::Rejected.is_terminal());
        assert!(!MergeStatus::Pending.is_terminal());
        assert!(!MergeStatus::Approved.is_terminal());
        assert!(!MergeStatus::Merging.is_terminal());
        assert!(!MergeStatus::Conflict.is_terminal());
        assert!(!MergeStatus::ChangesRequested.is_terminal());
    }

    #[test]
    fn conflict_type_is_mechanical() {
        assert!(ConflictType::NeedsRebase.is_mechanical());
        assert!(ConflictType::TrivialMerge.is_mechanical());
        assert!(!ConflictType::SourceConflict.is_mechanical());
        assert!(!ConflictType::ComplexConflict.is_mechanical());
        assert!(!ConflictType::Unknown.is_mechanical());
    }

    #[test]
    fn conflict_info_new() {
        let info = ConflictInfo::new(ConflictType::SourceConflict, "test description");
        assert_eq!(info.conflict_type, ConflictType::SourceConflict);
        assert_eq!(info.description, "test description");
        assert!(info.conflicting_files.is_empty());
    }

    #[test]
    fn conflict_info_with_files() {
        let info = ConflictInfo::new(ConflictType::SourceConflict, "test")
            .with_files(vec!["src/main.rs".to_string(), "Cargo.toml".to_string()]);
        assert_eq!(info.conflicting_files.len(), 2);
        assert_eq!(info.conflicting_files[0], "src/main.rs");
    }

    #[test]
    fn merge_queue_entry_with_conflict_info() {
        let mut entry = MergeQueueEntry::new("mq-1", "task-1", "https://example.com/pr/1");
        assert!(entry.conflict_info.is_none());

        entry.conflict_info = Some(ConflictInfo::new(ConflictType::NeedsRebase, "behind base"));
        assert!(entry.conflict_info.is_some());
        assert_eq!(
            entry.conflict_info.as_ref().unwrap().conflict_type,
            ConflictType::NeedsRebase
        );
    }

    #[test]
    fn conflict_info_serializes() {
        let info = ConflictInfo::new(ConflictType::SourceConflict, "test")
            .with_files(vec!["src/lib.rs".to_string()]);
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("source_conflict"));
        assert!(json.contains("src/lib.rs"));

        let deserialized: ConflictInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.conflict_type, ConflictType::SourceConflict);
    }
}
