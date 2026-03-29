//! Repository poller — spec github.md §5.
//!
//! Tracks a high-water mark per repository and fetches only items
//! updated since the last successful poll.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::client::GitHubClient;
use crate::error::GitHubError;
use crate::model::{Issue, IssueFilters, PullRequest, PullRequestFilters, RateLimit};

/// Result of a poll operation.
#[derive(Debug)]
pub struct PollResult {
    /// Issues that were created or updated since the last poll.
    pub issues: Vec<Issue>,
    /// Pull requests that were created or updated since the last poll.
    pub pull_requests: Vec<PullRequest>,
    /// The high-water mark timestamp from this poll.
    pub timestamp: Option<DateTime<Utc>>,
    /// Rate limit state after this poll.
    pub rate_limit: Option<RateLimit>,
}

/// Polls a single repository for new and changed issues and PRs (spec github.md §5.1).
///
/// The poller does not own a timer. The caller (scheduler) invokes `poll()` on
/// whatever cadence it chooses.
pub struct RepoPoller {
    client: GitHubClient,
    owner: String,
    repo: String,
    /// High-water mark: the most recent `updated_at` seen.
    /// `None` on first poll (fetches all open items).
    since: Option<DateTime<Utc>>,
}

impl RepoPoller {
    /// Create a new poller for the given repository.
    pub fn new(client: GitHubClient, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            client,
            owner: owner.into(),
            repo: repo.into(),
            since: None,
        }
    }

    /// Set the initial high-water mark (for restoring from persisted state).
    ///
    /// Call this before the first `poll()` to avoid a cold start that fetches
    /// all open items. Typically used with a timestamp loaded from the database
    /// after a server restart.
    pub fn with_since(mut self, since: Option<DateTime<Utc>>) -> Self {
        self.since = since;
        self
    }

    /// The current high-water mark.
    pub fn since(&self) -> Option<DateTime<Utc>> {
        self.since
    }

    /// Poll for both issues and PRs updated since the last poll.
    ///
    /// On first call, fetches all open items. On subsequent calls, only items
    /// with `updated_at` after the high-water mark.
    ///
    /// Issues and PRs are fetched concurrently to halve the per-poll latency.
    ///
    /// If the poll fails, the high-water mark is **not** advanced — the next
    /// poll retries the same window.
    pub async fn poll(&mut self) -> Result<PollResult, GitHubError> {
        let (issues, pull_requests) = tokio::try_join!(
            self.poll_issues_inner(),
            self.poll_pull_requests_inner(),
        )?;

        // Deduplicate by node_id to guard against duplicates from paginated
        // fetches (e.g. items shifting between cursor pages during a poll).
        let issues = Self::dedup_issues(issues);
        let pull_requests = Self::dedup_pull_requests(pull_requests);

        // Advance high-water mark to the max updated_at across all returned items.
        let max_ts = issues
            .iter()
            .map(|i| i.updated_at)
            .chain(pull_requests.iter().map(|p| p.updated_at))
            .max();

        if let Some(ts) = max_ts {
            self.since = Some(ts);
        }

        Ok(PollResult {
            issues,
            pull_requests,
            timestamp: self.since,
            rate_limit: self.client.rate_limit(),
        })
    }

    /// Poll for issues only.
    pub async fn poll_issues(&mut self) -> Result<Vec<Issue>, GitHubError> {
        let issues = Self::dedup_issues(self.poll_issues_inner().await?);

        let max_ts = issues.iter().map(|i| i.updated_at).max();
        if let Some(ts) = max_ts {
            // Only advance if this is newer than existing mark.
            match self.since {
                Some(existing) if ts > existing => self.since = Some(ts),
                None => self.since = Some(ts),
                _ => {}
            }
        }

        Ok(issues)
    }

    /// Poll for pull requests only.
    pub async fn poll_pull_requests(&mut self) -> Result<Vec<PullRequest>, GitHubError> {
        let prs = Self::dedup_pull_requests(self.poll_pull_requests_inner().await?);

        let max_ts = prs.iter().map(|p| p.updated_at).max();
        if let Some(ts) = max_ts {
            match self.since {
                Some(existing) if ts > existing => self.since = Some(ts),
                None => self.since = Some(ts),
                _ => {}
            }
        }

        Ok(prs)
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    /// Deduplicate issues by `node_id`, keeping the first occurrence.
    fn dedup_issues(issues: Vec<Issue>) -> Vec<Issue> {
        let mut seen = HashSet::with_capacity(issues.len());
        issues
            .into_iter()
            .filter(|i| seen.insert(i.node_id.clone()))
            .collect()
    }

    /// Deduplicate pull requests by `node_id`, keeping the first occurrence.
    fn dedup_pull_requests(prs: Vec<PullRequest>) -> Vec<PullRequest> {
        let mut seen = HashSet::with_capacity(prs.len());
        prs.into_iter()
            .filter(|p| seen.insert(p.node_id.clone()))
            .collect()
    }

    async fn poll_issues_inner(&self) -> Result<Vec<Issue>, GitHubError> {
        // Include both Open and Closed states so we can detect external closures.
        // When an issue is closed, its updated_at changes and we'll see it in the poll.
        let filters = IssueFilters {
            states: Some(vec![
                crate::model::IssueState::Open,
                crate::model::IssueState::Closed,
            ]),
            since: self.since,
            ..Default::default()
        };
        self.client
            .list_issues(&self.owner, &self.repo, &filters)
            .await
    }

    async fn poll_pull_requests_inner(&self) -> Result<Vec<PullRequest>, GitHubError> {
        // Include all states (Open, Closed, Merged) so we can detect external closures.
        // When a PR is closed or merged, its updated_at changes and we'll see it in the poll.
        let filters = PullRequestFilters {
            states: Some(vec![
                crate::model::PullRequestState::Open,
                crate::model::PullRequestState::Closed,
                crate::model::PullRequestState::Merged,
            ]),
            since: self.since,
        };
        self.client
            .list_pull_requests(&self.owner, &self.repo, &filters)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueState, PullRequestState, User};
    use chrono::Utc;

    fn make_issue(node_id: &str, number: u64) -> Issue {
        let now = Utc::now();
        Issue {
            owner: "o".into(),
            repo: "r".into(),
            number,
            node_id: node_id.into(),
            title: format!("Issue {number}"),
            body: None,
            state: IssueState::Open,
            state_reason: None,
            labels: vec![],
            assignees: vec![],
            milestone: None,
            comments: vec![],
            parent: None,
            sub_issues: vec![],
            blocked_by: vec![],
            linked_pull_requests: vec![],
            author: User {
                login: "u".into(),
                node_id: "u1".into(),
            },
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    }

    fn make_pr(node_id: &str, number: u64) -> PullRequest {
        let now = Utc::now();
        PullRequest {
            owner: "o".into(),
            repo: "r".into(),
            number,
            node_id: node_id.into(),
            title: format!("PR {number}"),
            body: None,
            state: PullRequestState::Open,
            head_ref: "feat".into(),
            head_sha: "abc".into(),
            base_ref: "main".into(),
            is_draft: false,
            mergeable: None,
            labels: vec![],
            assignees: vec![],
            review_decision: None,
            reviews: vec![],
            comments: vec![],
            linked_issues: vec![],
            author: User {
                login: "u".into(),
                node_id: "u1".into(),
            },
            created_at: now,
            updated_at: now,
            closed_at: None,
            merged_at: None,
        }
    }

    #[test]
    fn dedup_issues_removes_duplicates() {
        let issues = vec![
            make_issue("A", 1),
            make_issue("B", 2),
            make_issue("A", 1), // duplicate
            make_issue("C", 3),
            make_issue("B", 2), // duplicate
        ];
        let deduped = RepoPoller::dedup_issues(issues);
        assert_eq!(deduped.len(), 3);
        assert_eq!(deduped[0].node_id, "A");
        assert_eq!(deduped[1].node_id, "B");
        assert_eq!(deduped[2].node_id, "C");
    }

    #[test]
    fn dedup_issues_no_duplicates() {
        let issues = vec![make_issue("A", 1), make_issue("B", 2)];
        let deduped = RepoPoller::dedup_issues(issues);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedup_issues_empty() {
        let deduped = RepoPoller::dedup_issues(vec![]);
        assert!(deduped.is_empty());
    }

    #[test]
    fn dedup_pull_requests_removes_duplicates() {
        let prs = vec![
            make_pr("X", 10),
            make_pr("Y", 11),
            make_pr("X", 10), // duplicate
        ];
        let deduped = RepoPoller::dedup_pull_requests(prs);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].node_id, "X");
        assert_eq!(deduped[1].node_id, "Y");
    }

    #[test]
    fn dedup_pull_requests_no_duplicates() {
        let prs = vec![make_pr("X", 10)];
        let deduped = RepoPoller::dedup_pull_requests(prs);
        assert_eq!(deduped.len(), 1);
    }
}
