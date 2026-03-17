//! Normalized GitHub model types — spec github.md §2.
//!
//! These types represent the stable internal model that the rest of the system
//! works with. They are decoupled from GitHub's API response shapes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A normalized GitHub issue (spec github.md §2.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// GitHub's global GraphQL node ID.
    pub node_id: String,
    pub title: String,
    pub body: Option<String>,
    pub state: IssueState,
    pub state_reason: Option<IssueStateReason>,
    pub labels: Vec<Label>,
    pub assignees: Vec<User>,
    pub milestone: Option<Milestone>,
    /// Full comment history, ordered chronologically.
    pub comments: Vec<Comment>,
    /// Parent issue if this is a sub-issue.
    pub parent: Option<ParentIssueRef>,
    /// Issues linked as sub-issues via GitHub's sub-issue feature.
    pub sub_issues: Vec<SubIssueRef>,
    /// Issues that block this issue (must be resolved before this issue can be worked on).
    pub blocked_by: Vec<BlockingIssueRef>,
    /// PRs that reference this issue.
    pub linked_pull_requests: Vec<LinkedPR>,
    pub author: User,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// A normalized GitHub pull request (spec github.md §2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub node_id: String,
    pub title: String,
    pub body: Option<String>,
    pub state: PullRequestState,
    /// Source branch name.
    pub head_ref: String,
    /// Current head commit SHA.
    pub head_sha: String,
    /// Target branch name.
    pub base_ref: String,
    pub is_draft: bool,
    pub mergeable: Option<MergeableState>,
    pub labels: Vec<Label>,
    pub assignees: Vec<User>,
    pub review_decision: Option<ReviewDecision>,
    /// All reviews, ordered chronologically.
    pub reviews: Vec<Review>,
    /// Issue-level comments (not review comments).
    pub comments: Vec<Comment>,
    /// Issues this PR closes/references.
    pub linked_issues: Vec<LinkedIssueRef>,
    pub author: User,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueStateReason {
    Completed,
    NotPlanned,
    Reopened,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MilestoneState {
    Open,
    Closed,
}

// ---------------------------------------------------------------------------
// Supporting types (spec github.md §2.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub login: String,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Milestone {
    pub title: String,
    pub number: u64,
    pub state: MilestoneState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub author: User,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub author: User,
    pub state: ReviewState,
    pub body: Option<String>,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubIssueRef {
    pub number: u64,
    pub title: String,
    pub state: IssueState,
    pub node_id: String,
}

/// Reference to a parent issue (GitHub's sub-issue feature).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParentIssueRef {
    pub number: u64,
    pub title: String,
    pub state: IssueState,
    pub node_id: String,
}

/// Reference to an issue that blocks another issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockingIssueRef {
    pub number: u64,
    pub title: String,
    pub state: IssueState,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPR {
    pub number: u64,
    pub title: String,
    pub state: PullRequestState,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedIssueRef {
    pub number: u64,
    pub title: String,
    pub state: IssueState,
    pub node_id: String,
}

// ---------------------------------------------------------------------------
// Filters (spec github.md §4.3)
// ---------------------------------------------------------------------------

/// Filters for listing issues.
#[derive(Debug, Clone, Default)]
pub struct IssueFilters {
    pub states: Option<Vec<IssueState>>,
    pub labels: Option<Vec<String>>,
    pub since: Option<DateTime<Utc>>,
}

/// Filters for listing pull requests.
#[derive(Debug, Clone, Default)]
pub struct PullRequestFilters {
    pub states: Option<Vec<PullRequestState>>,
    pub since: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Rate limit (spec github.md §3.5)
// ---------------------------------------------------------------------------

/// Current rate limit state, parsed from response headers.
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    /// Points remaining in the current window.
    pub remaining: u32,
    /// When the rate limit resets.
    pub reset_at: DateTime<Utc>,
}
