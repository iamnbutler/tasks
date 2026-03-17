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

    /// Parse the PR URL to extract owner, repo, and PR number.
    ///
    /// Expected format: `https://github.com/{owner}/{repo}/pull/{number}`
    ///
    /// Returns `None` if the URL doesn't match the expected format.
    pub fn parse_pr_url(&self) -> Option<PrRef> {
        parse_github_pr_url(&self.pr_url)
    }
}

/// Parsed reference to a GitHub pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

/// Parse a GitHub PR URL into its components.
///
/// Expected format: `https://github.com/{owner}/{repo}/pull/{number}`
///
/// Returns `None` if the URL doesn't match the expected format.
pub fn parse_github_pr_url(url: &str) -> Option<PrRef> {
    // Remove trailing slash if present
    let url = url.trim_end_matches('/');

    // Expected: https://github.com/{owner}/{repo}/pull/{number}
    let url = url.strip_prefix("https://github.com/")?;

    let parts: Vec<&str> = url.split('/').collect();
    if parts.len() != 4 {
        return None;
    }

    if parts[2] != "pull" {
        return None;
    }

    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let number: u64 = parts[3].parse().ok()?;

    Some(PrRef { owner, repo, number })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_pr_url() {
        let url = "https://github.com/owner/repo/pull/123";
        let pr_ref = parse_github_pr_url(url).unwrap();
        assert_eq!(pr_ref.owner, "owner");
        assert_eq!(pr_ref.repo, "repo");
        assert_eq!(pr_ref.number, 123);
    }

    #[test]
    fn parse_pr_url_with_trailing_slash() {
        let url = "https://github.com/owner/repo/pull/456/";
        let pr_ref = parse_github_pr_url(url).unwrap();
        assert_eq!(pr_ref.number, 456);
    }

    #[test]
    fn parse_invalid_urls() {
        assert!(parse_github_pr_url("https://github.com/owner/repo/issues/123").is_none());
        assert!(parse_github_pr_url("https://gitlab.com/owner/repo/pull/123").is_none());
        assert!(parse_github_pr_url("https://github.com/owner/repo/pull/abc").is_none());
        assert!(parse_github_pr_url("not a url").is_none());
        assert!(parse_github_pr_url("").is_none());
    }

    #[test]
    fn entry_parse_pr_url() {
        let entry = MergeQueueEntry::new("e1", "t1", "https://github.com/foo/bar/pull/42");
        let pr_ref = entry.parse_pr_url().unwrap();
        assert_eq!(pr_ref.owner, "foo");
        assert_eq!(pr_ref.repo, "bar");
        assert_eq!(pr_ref.number, 42);
    }
}
