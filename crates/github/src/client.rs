//! GitHub GraphQL client — spec github.md §4.

use std::sync::RwLock;

use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::GitHubError;
use crate::model::{
    Issue, IssueFilters, IssueState, MergeableState, PrMergeStatus, PullRequest,
    PullRequestFilters, PullRequestState, RateLimit,
};
use crate::queries;
use crate::response::*;

/// Response from GitHub REST API when creating an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedIssue {
    /// Issue number.
    pub number: u64,
    /// Issue title.
    pub title: String,
    /// Issue body (markdown).
    pub body: Option<String>,
    /// Issue URL (HTML).
    pub html_url: String,
    /// Issue state.
    pub state: String,
}

/// Response from GitHub REST API when creating a repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedRepository {
    /// Repository ID.
    pub id: u64,
    /// Repository name (e.g., "my-project").
    pub name: String,
    /// Full repository name (e.g., "owner/my-project").
    pub full_name: String,
    /// Repository URL (HTML).
    pub html_url: String,
    /// Default branch name.
    pub default_branch: String,
    /// Whether the repository is private.
    pub private: bool,
}

/// Maximum items per page (GitHub's limit).
const DEFAULT_PAGE_SIZE: u32 = 100;

/// Default maximum pages to fetch before stopping.
const DEFAULT_MAX_PAGES: u32 = 10;

/// Default rate limit floor — pause requests below this threshold.
const DEFAULT_RATE_LIMIT_FLOOR: u32 = 200;

/// GitHub GraphQL API client (spec github.md §4).
pub struct GitHubClient {
    http: reqwest::Client,
    base_url: String,
    max_pages: u32,
    rate_limit_floor: u32,
    rate_limit: RwLock<Option<RateLimit>>,
}

/// Builder for constructing a [`GitHubClient`] with custom settings.
pub struct GitHubClientBuilder {
    token: String,
    base_url: String,
    max_pages: u32,
    rate_limit_floor: u32,
}

impl GitHubClientBuilder {
    /// Override the GitHub API base URL (for GitHub Enterprise or testing).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Maximum number of pages to fetch per query (default: 10).
    pub fn max_pages(mut self, max: u32) -> Self {
        self.max_pages = max;
        self
    }

    /// Minimum remaining rate limit points before pausing (default: 200).
    pub fn rate_limit_floor(mut self, floor: u32) -> Self {
        self.rate_limit_floor = floor;
        self
    }

    /// Build the client.
    pub fn build(self) -> GitHubClient {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .expect("invalid token characters"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("tasks-github"));
        // Required for sub-issues API access (public preview feature).
        headers.insert(
            "GraphQL-Features",
            HeaderValue::from_static("sub_issues"),
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("failed to build HTTP client");

        GitHubClient {
            http,
            base_url: self.base_url,
            max_pages: self.max_pages,
            rate_limit_floor: self.rate_limit_floor,
            rate_limit: RwLock::new(None),
        }
    }
}

impl GitHubClient {
    /// Create a new client with a personal access token.
    pub fn new(token: impl Into<String>) -> Self {
        Self::builder(token).build()
    }

    /// Create a builder for customizing the client.
    pub fn builder(token: impl Into<String>) -> GitHubClientBuilder {
        GitHubClientBuilder {
            token: token.into(),
            base_url: "https://api.github.com".to_string(),
            max_pages: DEFAULT_MAX_PAGES,
            rate_limit_floor: DEFAULT_RATE_LIMIT_FLOOR,
        }
    }

    /// Current rate limit state (if known from a prior request).
    pub fn rate_limit(&self) -> Option<RateLimit> {
        *self.rate_limit.read().unwrap()
    }

    // -----------------------------------------------------------------------
    // Issues (spec github.md §4.2)
    // -----------------------------------------------------------------------

    /// List issues for a repository with optional filters.
    /// Paginates automatically up to `max_pages`.
    pub async fn list_issues(
        &self,
        owner: &str,
        repo: &str,
        filters: &IssueFilters,
    ) -> Result<Vec<Issue>, GitHubError> {
        let query = queries::list_issues_query();
        let states = filters
            .states
            .as_ref()
            .map(|s| s.iter().map(|st| issue_state_gql(*st)).collect::<Vec<_>>());
        let since = filters.since.map(|dt| dt.to_rfc3339());

        let mut all_issues = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..self.max_pages {
            let variables = json!({
                "owner": owner,
                "name": repo,
                "first": DEFAULT_PAGE_SIZE,
                "after": cursor,
                "states": states,
                "labels": filters.labels,
                "since": since,
            });

            let resp: GraphQLResponse<ListIssuesData> =
                self.execute(&query, variables).await?;
            let data = self.unwrap_data(resp)?;
            let repo_data = data
                .repository
                .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}")))?;

            let page = repo_data.issues;
            let has_next = page.page_info.has_next_page;
            cursor = page.page_info.end_cursor;

            for gql_issue in page.nodes {
                let needs_more_comments = gql_issue.has_more_comments();
                let comment_cursor = gql_issue.comments_cursor().map(String::from);
                let number = gql_issue.number;
                let mut issue = gql_issue.into_model(owner, repo);

                if needs_more_comments {
                    if let Some(c) = comment_cursor {
                        self.fetch_remaining_issue_comments(
                            owner,
                            repo,
                            number,
                            c,
                            &mut issue.comments,
                        )
                        .await?;
                    }
                }

                all_issues.push(issue);
            }

            if !has_next || cursor.is_none() {
                break;
            }
        }

        Ok(all_issues)
    }

    /// Fetch a single issue by number with full detail.
    pub async fn get_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Issue, GitHubError> {
        let query = queries::get_issue_query();
        let variables = json!({
            "owner": owner,
            "name": repo,
            "number": number as i64,
        });

        let resp: GraphQLResponse<GetIssueData> = self.execute(&query, variables).await?;
        let data = self.unwrap_data(resp)?;
        let repo_data = data
            .repository
            .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}")))?;
        let gql_issue = repo_data
            .issue
            .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}#{number}")))?;

        let needs_more_comments = gql_issue.has_more_comments();
        let comment_cursor = gql_issue.comments_cursor().map(String::from);
        let mut issue = gql_issue.into_model(owner, repo);

        if needs_more_comments {
            if let Some(c) = comment_cursor {
                self.fetch_remaining_issue_comments(owner, repo, number, c, &mut issue.comments)
                    .await?;
            }
        }

        Ok(issue)
    }

    /// Create a new issue in a repository.
    ///
    /// Uses the GitHub REST API to create an issue. Returns the created issue
    /// with its assigned number and other populated fields.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: Option<&str>,
        labels: Option<&[String]>,
    ) -> Result<CreatedIssue, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!("{}/repos/{}/{}/issues", self.base_url, owner, repo);

        let mut request_body = json!({
            "title": title,
        });

        if let Some(b) = body {
            request_body["body"] = json!(b);
        }

        if let Some(l) = labels {
            request_body["labels"] = json!(l);
        }

        let response = self.http.post(&url).json(&request_body).send().await?;
        self.update_rate_limit(&response);

        let status = response.status();

        if status == reqwest::StatusCode::CREATED {
            let created: CreatedIssue = response.json().await.map_err(|e| {
                GitHubError::Decode(format!("failed to parse created issue: {e}"))
            })?;
            return Ok(created);
        }

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GitHubError::NotFound(format!("{owner}/{repo}")));
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Validation(text));
        }

        let text = response.text().await.unwrap_or_default();
        Err(GitHubError::Decode(format!(
            "unexpected create issue status {status}: {text}"
        )))
    }

    // -----------------------------------------------------------------------
    // Pull Requests (spec github.md §4.2)
    // -----------------------------------------------------------------------

    /// List pull requests for a repository with optional filters.
    /// Paginates automatically up to `max_pages`.
    pub async fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        filters: &PullRequestFilters,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        let query = queries::list_pull_requests_query();
        let states = filters.states.as_ref().map(|s| {
            s.iter()
                .map(|st| pr_state_gql(*st))
                .collect::<Vec<_>>()
        });

        let mut all_prs = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..self.max_pages {
            let variables = json!({
                "owner": owner,
                "name": repo,
                "first": DEFAULT_PAGE_SIZE,
                "after": cursor,
                "states": states,
            });

            let resp: GraphQLResponse<ListPullRequestsData> =
                self.execute(&query, variables).await?;
            let data = self.unwrap_data(resp)?;
            let repo_data = data
                .repository
                .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}")))?;

            let page = repo_data.pull_requests;
            let has_next = page.page_info.has_next_page;
            cursor = page.page_info.end_cursor;

            // For PRs we don't have a `since` filter in GraphQL, so we stop
            // paginating when we hit PRs older than our `since` threshold.
            let mut hit_cutoff = false;

            for gql_pr in page.nodes {
                if let Some(since) = filters.since {
                    if gql_pr.updated_at < since {
                        hit_cutoff = true;
                        break;
                    }
                }

                let needs_more_comments = gql_pr.has_more_comments();
                let comment_cursor = gql_pr.comments_cursor().map(String::from);
                let needs_more_reviews = gql_pr.has_more_reviews();
                let review_cursor = gql_pr.reviews_cursor().map(String::from);
                let number = gql_pr.number;
                let mut pr = gql_pr.into_model(owner, repo);

                if needs_more_comments {
                    if let Some(c) = comment_cursor {
                        self.fetch_remaining_pr_comments(
                            owner,
                            repo,
                            number,
                            c,
                            &mut pr.comments,
                        )
                        .await?;
                    }
                }

                if needs_more_reviews {
                    if let Some(c) = review_cursor {
                        self.fetch_remaining_pr_reviews(
                            owner,
                            repo,
                            number,
                            c,
                            &mut pr.reviews,
                        )
                        .await?;
                    }
                }

                all_prs.push(pr);
            }

            if hit_cutoff || !has_next || cursor.is_none() {
                break;
            }
        }

        Ok(all_prs)
    }

    /// Fetch a single pull request by number with full detail.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        let query = queries::get_pull_request_query();
        let variables = json!({
            "owner": owner,
            "name": repo,
            "number": number as i64,
        });

        let resp: GraphQLResponse<GetPullRequestData> =
            self.execute(&query, variables).await?;
        let data = self.unwrap_data(resp)?;
        let repo_data = data
            .repository
            .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}")))?;
        let gql_pr = repo_data
            .pull_request
            .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}#{number}")))?;

        let needs_more_comments = gql_pr.has_more_comments();
        let comment_cursor = gql_pr.comments_cursor().map(String::from);
        let needs_more_reviews = gql_pr.has_more_reviews();
        let review_cursor = gql_pr.reviews_cursor().map(String::from);
        let mut pr = gql_pr.into_model(owner, repo);

        if needs_more_comments {
            if let Some(c) = comment_cursor {
                self.fetch_remaining_pr_comments(owner, repo, number, c, &mut pr.comments)
                    .await?;
            }
        }

        if needs_more_reviews {
            if let Some(c) = review_cursor {
                self.fetch_remaining_pr_reviews(owner, repo, number, c, &mut pr.reviews)
                    .await?;
            }
        }

        Ok(pr)
    }

    // -----------------------------------------------------------------------
    // File content (spec §14 — workflow config loading)
    // -----------------------------------------------------------------------

    /// Fetch file content from a repository at a given path.
    ///
    /// Uses the GitHub REST API contents endpoint. Returns the file's decoded
    /// content as a string. The path should be relative to the repo root
    /// (e.g., "workflow.toml" or ".tasks/prompt.md").
    ///
    /// Returns `Ok(None)` if the file doesn't exist (404). Returns an error
    /// for other failures (network, auth, etc.).
    pub async fn get_file_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
    ) -> Result<Option<String>, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!(
            "{}/repos/{}/{}/contents/{}",
            self.base_url, owner, repo, path
        );

        let response = self
            .http
            .get(&url)
            .header("Accept", "application/vnd.github.raw+json")
            .send()
            .await?;

        // Update rate limit state from headers.
        self.update_rate_limit(&response);

        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Decode(format!(
                "unexpected status {status}: {text}"
            )));
        }

        let content = response.text().await.map_err(|e| {
            GitHubError::Decode(format!("failed to read response body: {e}"))
        })?;

        Ok(Some(content))
    }

    /// Fetch raw file content at a specific git ref (branch, tag, or SHA).
    ///
    /// Like `get_file_content` but targets a specific ref instead of the
    /// default branch.
    pub async fn get_file_content_at_ref(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!(
            "{}/repos/{}/{}/contents/{}?ref={}",
            self.base_url, owner, repo, path, git_ref
        );

        let response = self
            .http
            .get(&url)
            .header("Accept", "application/vnd.github.raw+json")
            .send()
            .await?;

        self.update_rate_limit(&response);

        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Decode(format!(
                "unexpected status {status}: {text}"
            )));
        }

        let content = response.text().await.map_err(|e| {
            GitHubError::Decode(format!("failed to read response body: {e}"))
        })?;

        Ok(Some(content))
    }

    // -----------------------------------------------------------------------
    // PR diff (REST API)
    // -----------------------------------------------------------------------

    /// Maximum diff size to return (100KB). Larger diffs are truncated.
    const MAX_DIFF_SIZE: usize = 100_000;

    /// Fetch the unified diff for a pull request.
    ///
    /// Uses the GitHub REST API with `Accept: application/vnd.github.diff`.
    /// Large diffs (>100KB) are truncated with a notice appended.
    pub async fn get_pr_diff(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<String, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.base_url, owner, repo, number
        );

        let response = self
            .http
            .get(&url)
            .header("Accept", "application/vnd.github.diff")
            .send()
            .await?;

        self.update_rate_limit(&response);

        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GitHubError::NotFound(format!(
                "{owner}/{repo}#{number}"
            )));
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Decode(format!(
                "unexpected status {status}: {text}"
            )));
        }

        let content = response.text().await.map_err(|e| {
            GitHubError::Decode(format!("failed to read response body: {e}"))
        })?;

        // Truncate large diffs to avoid blowing out LLM context
        if content.len() > Self::MAX_DIFF_SIZE {
            let truncated = &content[..Self::MAX_DIFF_SIZE];
            // Try to cut at a line boundary
            let cut_at = truncated.rfind('\n').unwrap_or(Self::MAX_DIFF_SIZE);
            Ok(format!(
                "{}\n\n[diff truncated — showing {cut_at} of {} bytes]",
                &content[..cut_at],
                content.len()
            ))
        } else {
            Ok(content)
        }
    }

    // -----------------------------------------------------------------------
    // PR merge (spec §7.1)
    // -----------------------------------------------------------------------

    /// Merge a pull request via the GitHub REST API.
    ///
    /// Uses squash merge by default. Returns `Ok(true)` if merged successfully,
    /// `Ok(false)` if the PR is not mergeable (conflicts, checks failing, etc.).
    pub async fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<bool, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!(
            "{}/repos/{}/{}/pulls/{}/merge",
            self.base_url, owner, repo, number
        );

        let body = json!({
            "merge_method": "squash",
        });

        let response = self.http.put(&url).json(&body).send().await?;
        self.update_rate_limit(&response);

        let status = response.status();

        if status.is_success() {
            return Ok(true);
        }

        // 405 = not mergeable (conflicts, checks required, etc.)
        // 409 = merge conflict
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            || status == reqwest::StatusCode::CONFLICT
        {
            return Ok(false);
        }

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GitHubError::NotFound(format!(
                "{owner}/{repo}#{number}"
            )));
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        let text = response.text().await.unwrap_or_default();
        Err(GitHubError::Decode(format!(
            "unexpected merge status {status}: {text}"
        )))
    }

    /// Check detailed PR merge status including conflict information.
    ///
    /// Returns a `PrMergeStatus` with:
    /// - Basic mergeable state
    /// - Whether the branch is behind base
    /// - Which files are conflicting (if any)
    ///
    /// This is used for conflict triage (spec §7.4) to decide whether
    /// conflicts can be resolved mechanically or need agent re-engagement.
    pub async fn check_pr_merge_status(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrMergeStatus, GitHubError> {
        // Use GraphQL to get detailed merge info including comparison
        let query = r#"
            query($owner: String!, $name: String!, $number: Int!) {
                repository(owner: $owner, name: $name) {
                    pullRequest(number: $number) {
                        mergeable
                        headRef {
                            name
                            compare(headRef: "HEAD") {
                                behindBy
                            }
                        }
                        baseRef {
                            name
                        }
                        files(first: 100) {
                            nodes {
                                path
                            }
                        }
                    }
                }
            }
        "#;

        let variables = json!({
            "owner": owner,
            "name": repo,
            "number": number as i64,
        });

        let resp: crate::response::GraphQLResponse<serde_json::Value> =
            self.execute(query, variables).await?;
        let data = self.unwrap_data(resp)?;

        let pr = data
            .get("repository")
            .and_then(|r| r.get("pullRequest"))
            .ok_or_else(|| GitHubError::NotFound(format!("{owner}/{repo}#{number}")))?;

        let mergeable_str = pr
            .get("mergeable")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN");

        let mergeable = match mergeable_str {
            "MERGEABLE" => MergeableState::Mergeable,
            "CONFLICTING" => MergeableState::Conflicting,
            _ => MergeableState::Unknown,
        };

        let head_ref = pr
            .get("headRef")
            .and_then(|h| h.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();

        let base_ref = pr
            .get("baseRef")
            .and_then(|b| b.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("main")
            .to_string();

        let commits_behind = pr
            .get("headRef")
            .and_then(|h| h.get("compare"))
            .and_then(|c| c.get("behindBy"))
            .and_then(|b| b.as_u64())
            .unwrap_or(0) as u32;

        // Get changed files (we'll need to determine conflicts separately)
        let changed_files: Vec<String> = pr
            .get("files")
            .and_then(|f| f.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("path"))
                    .filter_map(|p| p.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        // For conflicting PRs, the changed files are potentially conflicting
        // (GitHub doesn't expose exact conflict info via API)
        let conflicting_files = if mergeable == MergeableState::Conflicting {
            changed_files
        } else {
            Vec::new()
        };

        Ok(PrMergeStatus {
            mergeable,
            behind_base_branch: commits_behind > 0,
            conflicting_files,
            head_ref,
            base_ref,
            commits_behind,
        })
    }

    // -----------------------------------------------------------------------
    // Branch management
    // -----------------------------------------------------------------------

    /// Delete a branch from the repository.
    ///
    /// Uses the GitHub REST API to delete a git ref. Returns `Ok(true)` if
    /// deleted successfully, `Ok(false)` if the branch doesn't exist (404).
    ///
    /// This is used when rejecting and re-dispatching a task to ensure the
    /// agent starts fresh without finding old work on the branch.
    pub async fn delete_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<bool, GitHubError> {
        self.wait_for_rate_limit().await;

        // GitHub REST API: DELETE /repos/{owner}/{repo}/git/refs/heads/{branch}
        let url = format!(
            "{}/repos/{}/{}/git/refs/heads/{}",
            self.base_url, owner, repo, branch
        );

        let response = self.http.delete(&url).send().await?;
        self.update_rate_limit(&response);

        let status = response.status();

        if status.is_success() || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(true);
        }

        // 404 = branch doesn't exist (not an error for our use case)
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            // Could be rate limiting or protected branch
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        let text = response.text().await.unwrap_or_default();
        Err(GitHubError::Decode(format!(
            "unexpected delete branch status {status}: {text}"
        )))
    }

    /// Update a PR's head branch with the base branch (spec §7.4).
    ///
    /// Uses GitHub's "Update Branch" API to bring the head branch up to date
    /// with the base branch. This can resolve simple conflicts by rebasing.
    ///
    /// Returns:
    /// - `Ok(true)` if updated successfully
    /// - `Ok(false)` if update failed (e.g., conflicts that can't be auto-resolved)
    /// - `Err` for API errors
    pub async fn update_branch(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<bool, GitHubError> {
        self.wait_for_rate_limit().await;

        // GitHub REST API: PUT /repos/{owner}/{repo}/pulls/{pull_number}/update-branch
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/update-branch",
            self.base_url, owner, repo, number
        );

        let response = self.http.put(&url).json(&serde_json::json!({})).send().await?;
        self.update_rate_limit(&response);

        let status = response.status();

        // 202 Accepted = update started successfully
        if status == reqwest::StatusCode::ACCEPTED {
            return Ok(true);
        }

        // 422 = can't update (conflicts, protected branch rules, etc.)
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(false);
        }

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GitHubError::NotFound(format!(
                "{owner}/{repo}#{number}"
            )));
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        let text = response.text().await.unwrap_or_default();
        Err(GitHubError::Decode(format!(
            "unexpected update-branch status {status}: {text}"
        )))
    }

    // -----------------------------------------------------------------------
    // PR discovery
    // -----------------------------------------------------------------------

    /// Find an open PR for a given head branch. Returns the PR URL if found.
    ///
    /// The platform does not create PRs — agents do that inside their sessions
    /// (spec §1). This method discovers PRs the agent created so the merge
    /// queue can link to them.
    pub async fn find_pr_for_branch(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<String>, GitHubError> {
        let query = r#"
            query($owner: String!, $name: String!, $head: String!) {
                repository(owner: $owner, name: $name) {
                    pullRequests(headRefName: $head, states: [OPEN], first: 1) {
                        nodes { url }
                    }
                }
            }
        "#;
        let variables = json!({ "owner": owner, "name": repo, "head": head });

        let resp: GraphQLResponse<serde_json::Value> =
            self.execute(query, variables).await?;
        let data = self.unwrap_data(resp)?;

        let url = data
            .get("repository")
            .and_then(|r| r.get("pullRequests"))
            .and_then(|prs| prs.get("nodes"))
            .and_then(|nodes| nodes.as_array())
            .and_then(|nodes| nodes.first())
            .and_then(|pr| pr.get("url"))
            .and_then(|u| u.as_str())
            .map(String::from);

        Ok(url)
    }

    // -----------------------------------------------------------------------
    // Repository management
    // -----------------------------------------------------------------------

    /// Create a new private repository for the authenticated user.
    ///
    /// Uses the GitHub REST API to create a repository. The repository is
    /// automatically initialized with a README so it can be cloned immediately.
    ///
    /// Returns the created repository information including owner and URL.
    pub async fn create_repository(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<CreatedRepository, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!("{}/user/repos", self.base_url);

        let mut request_body = json!({
            "name": name,
            "private": true,
            "auto_init": true,
        });

        if let Some(desc) = description {
            request_body["description"] = json!(desc);
        }

        let response = self.http.post(&url).json(&request_body).send().await?;
        self.update_rate_limit(&response);

        let status = response.status();

        if status == reqwest::StatusCode::CREATED {
            let created: CreatedRepository = response.json().await.map_err(|e| {
                GitHubError::Decode(format!("failed to parse created repository: {e}"))
            })?;
            return Ok(created);
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Validation(text));
        }

        let text = response.text().await.unwrap_or_default();
        Err(GitHubError::Decode(format!(
            "unexpected create repository status {status}: {text}"
        )))
    }

    /// Get the login of the authenticated user (`GET /user`).
    pub async fn get_authenticated_user_login(&self) -> Result<String, GitHubError> {
        self.wait_for_rate_limit().await;
        let url = format!("{}/user", self.base_url);
        let response = self.http.get(&url).send().await?;
        self.update_rate_limit(&response);
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(format!("GET /user failed ({status}): {text}")));
        }
        #[derive(Deserialize)]
        struct UserResponse { login: String }
        let user: UserResponse = response.json().await.map_err(|e| {
            GitHubError::Decode(format!("failed to parse user response: {e}"))
        })?;
        Ok(user.login)
    }

    // -----------------------------------------------------------------------
    // Nested pagination helpers
    // -----------------------------------------------------------------------

    async fn fetch_remaining_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        mut cursor: String,
        comments: &mut Vec<crate::model::Comment>,
    ) -> Result<(), GitHubError> {
        let query = queries::issue_comments_query();

        for _ in 0..self.max_pages {
            let variables = json!({
                "owner": owner,
                "name": repo,
                "number": number as i64,
                "first": DEFAULT_PAGE_SIZE,
                "after": cursor,
            });

            let resp: GraphQLResponse<IssueCommentsData> =
                self.execute(query, variables).await?;
            let data = self.unwrap_data(resp)?;
            let page = data
                .repository
                .and_then(|r| r.issue)
                .map(|i| i.comments)
                .ok_or_else(|| {
                    GitHubError::Decode("missing comment page data".to_string())
                })?;

            comments.extend(page.nodes.into_iter().map(GqlComment::into_model));

            if !page.page_info.has_next_page {
                break;
            }
            match page.page_info.end_cursor {
                Some(c) => cursor = c,
                None => break,
            }
        }

        Ok(())
    }

    async fn fetch_remaining_pr_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        mut cursor: String,
        comments: &mut Vec<crate::model::Comment>,
    ) -> Result<(), GitHubError> {
        let query = queries::pr_comments_query();

        for _ in 0..self.max_pages {
            let variables = json!({
                "owner": owner,
                "name": repo,
                "number": number as i64,
                "first": DEFAULT_PAGE_SIZE,
                "after": cursor,
            });

            let resp: GraphQLResponse<PrCommentsData> =
                self.execute(query, variables).await?;
            let data = self.unwrap_data(resp)?;
            let page = data
                .repository
                .and_then(|r| r.pull_request)
                .map(|p| p.comments)
                .ok_or_else(|| {
                    GitHubError::Decode("missing comment page data".to_string())
                })?;

            comments.extend(page.nodes.into_iter().map(GqlComment::into_model));

            if !page.page_info.has_next_page {
                break;
            }
            match page.page_info.end_cursor {
                Some(c) => cursor = c,
                None => break,
            }
        }

        Ok(())
    }

    async fn fetch_remaining_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        mut cursor: String,
        reviews: &mut Vec<crate::model::Review>,
    ) -> Result<(), GitHubError> {
        let query = queries::pr_reviews_query();

        for _ in 0..self.max_pages {
            let variables = json!({
                "owner": owner,
                "name": repo,
                "number": number as i64,
                "first": DEFAULT_PAGE_SIZE,
                "after": cursor,
            });

            let resp: GraphQLResponse<PrReviewsData> =
                self.execute(query, variables).await?;
            let data = self.unwrap_data(resp)?;
            let page = data
                .repository
                .and_then(|r| r.pull_request)
                .map(|p| p.reviews)
                .ok_or_else(|| {
                    GitHubError::Decode("missing review page data".to_string())
                })?;

            for r in page.nodes {
                let state = match r.state.as_str() {
                    "APPROVED" => crate::model::ReviewState::Approved,
                    "CHANGES_REQUESTED" => crate::model::ReviewState::ChangesRequested,
                    "DISMISSED" => crate::model::ReviewState::Dismissed,
                    _ => crate::model::ReviewState::Commented,
                };
                reviews.push(crate::model::Review {
                    id: r.id,
                    author: r
                        .author
                        .map(GqlUser::into_model)
                        .unwrap_or_else(|| crate::model::User {
                            login: "ghost".to_string(),
                            node_id: String::new(),
                        }),
                    state,
                    body: r.body,
                    submitted_at: r.submitted_at,
                });
            }

            if !page.page_info.has_next_page {
                break;
            }
            match page.page_info.end_cursor {
                Some(c) => cursor = c,
                None => break,
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // GraphQL execution
    // -----------------------------------------------------------------------

    /// Execute a GraphQL query and deserialize the response.
    async fn execute<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<GraphQLResponse<T>, GitHubError> {
        self.wait_for_rate_limit().await;

        let url = format!("{}/graphql", self.base_url);
        let body = json!({
            "query": query,
            "variables": variables,
        });

        let response = self.http.post(&url).json(&body).send().await?;

        // Update rate limit state from headers.
        self.update_rate_limit(&response);

        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            // Could be rate limiting.
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    // Wait and retry once.
                    self.sleep_until(rl.reset_at).await;
                    return self.execute_once(query, variables).await;
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Decode(format!(
                "unexpected status {status}: {text}"
            )));
        }

        let gql_response: GraphQLResponse<T> = response.json().await.map_err(|e| {
            GitHubError::Decode(format!("failed to parse response: {e}"))
        })?;

        Ok(gql_response)
    }

    /// Execute without rate-limit retry (used for the retry attempt itself).
    async fn execute_once<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<GraphQLResponse<T>, GitHubError> {
        let url = format!("{}/graphql", self.base_url);
        let body = json!({
            "query": query,
            "variables": variables,
        });

        let response = self.http.post(&url).json(&body).send().await?;
        self.update_rate_limit(&response);

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rl) = self.rate_limit() {
                if rl.remaining == 0 {
                    return Err(GitHubError::RateLimited {
                        reset_at: rl.reset_at,
                    });
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Auth(text));
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(GitHubError::Decode(format!(
                "unexpected status {status}: {text}"
            )));
        }

        let gql_response: GraphQLResponse<T> = response.json().await.map_err(|e| {
            GitHubError::Decode(format!("failed to parse response: {e}"))
        })?;

        Ok(gql_response)
    }

    /// Unwrap a GraphQL response, surfacing any GraphQL-level errors.
    fn unwrap_data<T>(&self, resp: GraphQLResponse<T>) -> Result<T, GitHubError> {
        if let Some(errors) = resp.errors {
            if !errors.is_empty() {
                // Check for NOT_FOUND type errors.
                if errors
                    .iter()
                    .any(|e| e.error_type.as_deref() == Some("NOT_FOUND"))
                {
                    return Err(GitHubError::NotFound(errors[0].message.clone()));
                }
                return Err(GitHubError::GraphQL(errors));
            }
        }
        resp.data
            .ok_or_else(|| GitHubError::Decode("response contained no data".to_string()))
    }

    /// Parse rate limit headers from a response.
    fn update_rate_limit(&self, response: &reqwest::Response) {
        let remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u32>().ok());

        let reset = response
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|ts| DateTime::from_timestamp(ts, 0));

        if let (Some(remaining), Some(reset_at)) = (remaining, reset) {
            *self.rate_limit.write().unwrap() = Some(RateLimit {
                remaining,
                reset_at,
            });
        }
    }

    /// If we're below the rate limit floor, wait until the reset window.
    async fn wait_for_rate_limit(&self) {
        let rl = *self.rate_limit.read().unwrap();
        if let Some(rl) = rl {
            if rl.remaining < self.rate_limit_floor {
                self.sleep_until(rl.reset_at).await;
            }
        }
    }

    /// Sleep until the given timestamp (or return immediately if it's in the past).
    async fn sleep_until(&self, until: DateTime<Utc>) {
        let now = Utc::now();
        if until > now {
            let duration = (until - now).to_std().unwrap_or_default();
            tokio::time::sleep(duration).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn issue_state_gql(state: IssueState) -> &'static str {
    match state {
        IssueState::Open => "OPEN",
        IssueState::Closed => "CLOSED",
    }
}

fn pr_state_gql(state: PullRequestState) -> &'static str {
    match state {
        PullRequestState::Open => "OPEN",
        PullRequestState::Closed => "CLOSED",
        PullRequestState::Merged => "MERGED",
    }
}
