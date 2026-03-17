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

    /// Parse the PR URL into (owner, repo, number).
    ///
    /// Expected format: `https://github.com/{owner}/{repo}/pull/{number}`
    pub fn parse_pr_url(&self) -> Option<(String, String, u64)> {
        parse_pr_url(&self.pr_url)
    }
}

/// Parse a GitHub PR URL into (owner, repo, number).
///
/// Expected format: `https://github.com/{owner}/{repo}/pull/{number}`
pub fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    // Handle both https://github.com/... and github.com/...
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("github.com/"))?;

    let parts: Vec<&str> = path.split('/').collect();
    // Expected: [owner, repo, "pull", number]
    if parts.len() < 4 || parts[2] != "pull" {
        return None;
    }

    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let number = parts[3].parse::<u64>().ok()?;

    Some((owner, repo, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_url_valid() {
        let url = "https://github.com/owner/repo/pull/123";
        let (owner, repo, number) = parse_pr_url(url).unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
        assert_eq!(number, 123);
    }

    #[test]
    fn parse_pr_url_no_scheme() {
        let url = "github.com/owner/repo/pull/456";
        let (owner, repo, number) = parse_pr_url(url).unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
        assert_eq!(number, 456);
    }

    #[test]
    fn parse_pr_url_invalid() {
        assert!(parse_pr_url("https://github.com/owner/repo").is_none());
        assert!(parse_pr_url("https://github.com/owner/repo/issues/123").is_none());
        assert!(parse_pr_url("not a url").is_none());
    }

    #[test]
    fn entry_parse_pr_url() {
        let entry = MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/42");
        let (owner, repo, number) = entry.parse_pr_url().unwrap();
        assert_eq!(owner, "test");
        assert_eq!(repo, "repo");
        assert_eq!(number, 42);
    }
}
