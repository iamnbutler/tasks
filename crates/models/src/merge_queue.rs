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
}

impl MergeQueueEntry {
    pub fn new(id: impl Into<String>, task_id: impl Into<String>, pr_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            pr_url: pr_url.into(),
            status: MergeStatus::Pending,
            queued_at: Utc::now(),
        }
    }
}
