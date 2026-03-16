//! Repository poller — spec github.md §5.
//!
//! Tracks a high-water mark per repository and fetches only items
//! updated since the last successful poll.

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

    /// The current high-water mark.
    pub fn since(&self) -> Option<DateTime<Utc>> {
        self.since
    }

    /// Poll for both issues and PRs updated since the last poll.
    ///
    /// On first call, fetches all open items. On subsequent calls, only items
    /// with `updated_at` after the high-water mark.
    ///
    /// If the poll fails, the high-water mark is **not** advanced — the next
    /// poll retries the same window.
    pub async fn poll(&mut self) -> Result<PollResult, GitHubError> {
        let issues = self.poll_issues_inner().await?;
        let pull_requests = self.poll_pull_requests_inner().await?;

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
        let issues = self.poll_issues_inner().await?;

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
        let prs = self.poll_pull_requests_inner().await?;

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

    async fn poll_issues_inner(&self) -> Result<Vec<Issue>, GitHubError> {
        let filters = IssueFilters {
            states: Some(vec![crate::model::IssueState::Open]),
            since: self.since,
            ..Default::default()
        };
        self.client
            .list_issues(&self.owner, &self.repo, &filters)
            .await
    }

    async fn poll_pull_requests_inner(&self) -> Result<Vec<PullRequest>, GitHubError> {
        let filters = PullRequestFilters {
            states: Some(vec![crate::model::PullRequestState::Open]),
            since: self.since,
        };
        self.client
            .list_pull_requests(&self.owner, &self.repo, &filters)
            .await
    }
}
