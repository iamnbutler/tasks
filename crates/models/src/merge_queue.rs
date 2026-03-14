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

/// A merge queue entry — spec Section 5.5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeQueueEntry {
    /// Queue entry ID.
    pub id: String,
    pub task_id: String,
    pub pr_url: Option<String>,
    pub status: MergeStatus,
    pub queued_at: DateTime<Utc>,
}

impl MergeQueueEntry {
    pub fn new(id: impl Into<String>, task_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            pr_url: None,
            status: MergeStatus::Pending,
            queued_at: Utc::now(),
        }
    }
}
