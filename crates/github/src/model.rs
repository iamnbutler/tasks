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

impl Issue {
    /// Classify why this issue was closed by combining `state_reason` with
    /// linked PR data. Returns `None` if the issue is not closed.
    ///
    /// This solves the ambiguity where `state_reason == Completed` could mean
    /// either "agent's PR was merged" or "maintainer manually closed it."
    pub fn classify_closure(&self) -> Option<ClosureReason> {
        if self.state != IssueState::Closed {
            return None;
        }

        let has_merged_pr = self
            .linked_pull_requests
            .iter()
            .any(|pr| pr.state == PullRequestState::Merged);

        Some(match self.state_reason {
            Some(IssueStateReason::Completed) if has_merged_pr => ClosureReason::PrMerged,
            Some(IssueStateReason::Completed) => ClosureReason::ManualCompletion,
            Some(IssueStateReason::NotPlanned) => ClosureReason::NotPlanned,
            // Reopened shouldn't appear on a closed issue, but handle gracefully
            Some(IssueStateReason::Reopened) => ClosureReason::Unknown,
            None => ClosureReason::Unknown,
        })
    }
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

/// Why a closed issue was closed — richer than `IssueStateReason` alone.
///
/// Combines `state_reason` with linked PR state to distinguish between:
/// - Agent's PR was merged (success feedback)
/// - Maintainer manually completed it (no merged PR)
/// - Maintainer rejected / closed as not-planned
/// - Closed with no state_reason at all
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureReason {
    /// A linked PR was merged, then the issue was closed. Agent work accepted.
    PrMerged,
    /// Closed as completed, but no linked PR was merged. Manual completion.
    ManualCompletion,
    /// Closed as not-planned. Maintainer decided not to pursue it (rejection/duplicate).
    NotPlanned,
    /// Closed without a `state_reason`. Ambiguous — treated as cancellation.
    Unknown,
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

/// Detailed PR merge status with conflict information.
///
/// This struct provides more detail than just `MergeableState`:
/// - Whether the branch is behind base (needs rebase)
/// - Which files are conflicting
/// - Whether conflicts are in generated files vs source
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrMergeStatus {
    /// Basic mergeable state from GitHub.
    pub mergeable: MergeableState,
    /// Whether the branch is behind the base branch.
    pub behind_base_branch: bool,
    /// Files with conflicts (if mergeable is Conflicting).
    pub conflicting_files: Vec<String>,
    /// The head ref name.
    pub head_ref: String,
    /// The base ref name.
    pub base_ref: String,
    /// Number of commits the branch is behind.
    pub commits_behind: u32,
}

impl PrMergeStatus {
    /// Returns true if this is a trivial conflict (only lock/generated files).
    pub fn is_trivial_conflict(&self) -> bool {
        if self.mergeable != MergeableState::Conflicting {
            return false;
        }
        self.conflicting_files.iter().all(Self::is_generated_file)
    }

    /// Check if a file is generated/lock file that can be auto-resolved.
    fn is_generated_file(path: &String) -> bool {
        let path_lower = path.to_lowercase();
        path_lower.ends_with("package-lock.json")
            || path_lower.ends_with("yarn.lock")
            || path_lower.ends_with("pnpm-lock.yaml")
            || path_lower.ends_with("cargo.lock")
            || path_lower.ends_with("go.sum")
            || path_lower.ends_with("poetry.lock")
            || path_lower.ends_with("composer.lock")
            || path_lower.ends_with("gemfile.lock")
            || path_lower.contains(".generated.")
            || path_lower.contains("/generated/")
            || path_lower.ends_with(".min.js")
            || path_lower.ends_with(".min.css")
    }

    /// Returns true if the conflict is complex (many files or source files).
    pub fn is_complex_conflict(&self) -> bool {
        // More than 5 conflicting files is complex
        self.conflicting_files.len() > 5
    }
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
    pub owner: String,
    pub repo: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_closed_issue(
        state_reason: Option<IssueStateReason>,
        linked_prs: Vec<LinkedPR>,
    ) -> Issue {
        let now = Utc::now();
        Issue {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number: 1,
            node_id: "I_1".to_string(),
            title: "Test".to_string(),
            body: None,
            state: IssueState::Closed,
            state_reason,
            labels: vec![],
            assignees: vec![],
            milestone: None,
            comments: vec![],
            parent: None,
            sub_issues: vec![],
            blocked_by: vec![],
            linked_pull_requests: linked_prs,
            author: User { login: "u".to_string(), node_id: "U_1".to_string() },
            created_at: now,
            updated_at: now,
            closed_at: Some(now),
        }
    }

    #[test]
    fn classify_closure_returns_none_for_open_issue() {
        let mut issue = make_closed_issue(None, vec![]);
        issue.state = IssueState::Open;
        assert!(issue.classify_closure().is_none());
    }

    #[test]
    fn classify_closure_pr_merged() {
        let issue = make_closed_issue(
            Some(IssueStateReason::Completed),
            vec![LinkedPR {
                number: 10,
                title: "Fix".to_string(),
                state: PullRequestState::Merged,
                node_id: "PR_10".to_string(),
            }],
        );
        assert_eq!(issue.classify_closure(), Some(ClosureReason::PrMerged));
    }

    #[test]
    fn classify_closure_manual_completion() {
        let issue = make_closed_issue(Some(IssueStateReason::Completed), vec![]);
        assert_eq!(issue.classify_closure(), Some(ClosureReason::ManualCompletion));
    }

    #[test]
    fn classify_closure_completed_with_open_pr_is_manual() {
        let issue = make_closed_issue(
            Some(IssueStateReason::Completed),
            vec![LinkedPR {
                number: 10,
                title: "Fix".to_string(),
                state: PullRequestState::Open,
                node_id: "PR_10".to_string(),
            }],
        );
        assert_eq!(issue.classify_closure(), Some(ClosureReason::ManualCompletion));
    }

    #[test]
    fn classify_closure_not_planned() {
        let issue = make_closed_issue(Some(IssueStateReason::NotPlanned), vec![]);
        assert_eq!(issue.classify_closure(), Some(ClosureReason::NotPlanned));
    }

    #[test]
    fn classify_closure_unknown_no_reason() {
        let issue = make_closed_issue(None, vec![]);
        assert_eq!(issue.classify_closure(), Some(ClosureReason::Unknown));
    }

    #[test]
    fn classify_closure_multiple_prs_one_merged() {
        // If any linked PR is merged, it counts as PrMerged.
        let issue = make_closed_issue(
            Some(IssueStateReason::Completed),
            vec![
                LinkedPR {
                    number: 10,
                    title: "First attempt".to_string(),
                    state: PullRequestState::Closed,
                    node_id: "PR_10".to_string(),
                },
                LinkedPR {
                    number: 11,
                    title: "Second attempt".to_string(),
                    state: PullRequestState::Merged,
                    node_id: "PR_11".to_string(),
                },
            ],
        );
        assert_eq!(issue.classify_closure(), Some(ClosureReason::PrMerged));
    }
}
