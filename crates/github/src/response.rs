//! Raw GraphQL response types and normalization into the model types.
//!
//! These types mirror GitHub's GraphQL response shape. They are deserialized
//! from JSON and then converted to the normalized model types in `model.rs`.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::GraphQLError;
use crate::model;

// ---------------------------------------------------------------------------
// Top-level response wrapper
// ---------------------------------------------------------------------------

/// Every GraphQL response has this shape.
#[derive(Debug, Deserialize)]
pub(crate) struct GraphQLResponse<T> {
    pub data: Option<T>,
    pub errors: Option<Vec<GraphQLError>>,
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub(crate) struct Connection<T> {
    pub page_info: PageInfo,
    #[serde(default = "Vec::new")]
    pub nodes: Vec<T>,
}

/// A connection without pagination info (for nested connections we fetch in full).
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub(crate) struct Nodes<T> {
    #[serde(default = "Vec::new")]
    pub nodes: Vec<T>,
}

// ---------------------------------------------------------------------------
// Shared field types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GqlUser {
    pub login: String,
    /// Node ID — comes from inline fragment (... on User { id }), so may be absent.
    #[serde(default)]
    pub id: Option<String>,
}

impl GqlUser {
    pub fn into_model(self) -> model::User {
        model::User {
            login: self.login,
            node_id: self.id.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GqlLabel {
    pub name: String,
    pub color: String,
}

impl GqlLabel {
    pub fn into_model(self) -> model::Label {
        model::Label {
            name: self.name,
            color: self.color,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlMilestone {
    pub title: String,
    pub number: u64,
    pub state: String,
}

impl GqlMilestone {
    pub fn into_model(self) -> model::Milestone {
        let state = match self.state.as_str() {
            "CLOSED" => model::MilestoneState::Closed,
            _ => model::MilestoneState::Open,
        };
        model::Milestone {
            title: self.title,
            number: self.number,
            state,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlComment {
    pub id: String,
    pub author: Option<GqlUser>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl GqlComment {
    pub fn into_model(self) -> model::Comment {
        model::Comment {
            id: self.id,
            author: self
                .author
                .map(GqlUser::into_model)
                .unwrap_or_else(ghost_user),
            body: self.body,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Issue response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ListIssuesData {
    pub repository: Option<ListIssuesRepo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListIssuesRepo {
    pub issues: Connection<GqlIssue>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetIssueData {
    pub repository: Option<GetIssueRepo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetIssueRepo {
    pub issue: Option<GqlIssue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlIssue {
    pub number: u64,
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub state_reason: Option<String>,
    pub author: Option<GqlUser>,
    pub labels: Option<Nodes<GqlLabel>>,
    pub assignees: Option<Nodes<GqlUser>>,
    pub milestone: Option<GqlMilestone>,
    pub comments: Connection<GqlComment>,
    pub parent: Option<GqlParentIssue>,
    pub sub_issues: Option<Nodes<GqlSubIssue>>,
    pub timeline_items: Option<Nodes<GqlTimelineItem>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GqlSubIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub id: String,
}

/// Parent issue reference (from sub-issue feature).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct GqlParentIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub id: String,
}

/// A timeline item — we request CROSS_REFERENCED_EVENT and blocking events.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlTimelineItem {
    /// Present for CrossReferencedEvent.
    pub source: Option<GqlTimelineSource>,
    /// Present for MarkedAsBlockedByEvent and UnmarkedAsBlockedByEvent.
    pub blocking_issue: Option<GqlBlockingIssue>,
}

/// The source of a cross-reference. We only care about PRs.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlTimelineSource {
    /// Present only if the source is a PullRequest.
    pub number: Option<u64>,
    pub title: Option<String>,
    pub state: Option<String>,
    pub id: Option<String>,
}

/// Blocking issue reference from timeline events.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct GqlBlockingIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub id: String,
}

impl GqlIssue {
    /// Convert to the normalized model type. Requires owner/repo context.
    pub fn into_model(self, owner: &str, repo: &str) -> model::Issue {
        let state = match self.state.as_str() {
            "CLOSED" => model::IssueState::Closed,
            _ => model::IssueState::Open,
        };

        let state_reason = self.state_reason.as_deref().and_then(|r| match r {
            "COMPLETED" => Some(model::IssueStateReason::Completed),
            "NOT_PLANNED" => Some(model::IssueStateReason::NotPlanned),
            "REOPENED" => Some(model::IssueStateReason::Reopened),
            _ => None,
        });

        let labels = self
            .labels
            .map(|n| n.nodes.into_iter().map(GqlLabel::into_model).collect())
            .unwrap_or_default();

        let assignees = self
            .assignees
            .map(|n| n.nodes.into_iter().map(GqlUser::into_model).collect())
            .unwrap_or_default();

        let comments = self
            .comments
            .nodes
            .into_iter()
            .map(GqlComment::into_model)
            .collect();

        let parent = self.parent.map(|p| model::ParentIssueRef {
            number: p.number,
            title: p.title,
            state: parse_issue_state(&p.state),
            node_id: p.id,
        });

        let sub_issues = self
            .sub_issues
            .map(|n| {
                n.nodes
                    .into_iter()
                    .map(|si| model::SubIssueRef {
                        number: si.number,
                        title: si.title,
                        state: parse_issue_state(&si.state),
                        node_id: si.id,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let (linked_pull_requests, blocked_by) = self
            .timeline_items
            .map(|tl| extract_timeline_relationships(tl.nodes))
            .unwrap_or_default();

        model::Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: self.number,
            node_id: self.id,
            title: self.title,
            body: self.body,
            state,
            state_reason,
            labels,
            assignees,
            milestone: self.milestone.map(GqlMilestone::into_model),
            comments,
            parent,
            sub_issues,
            blocked_by,
            linked_pull_requests,
            author: self
                .author
                .map(GqlUser::into_model)
                .unwrap_or_else(ghost_user),
            created_at: self.created_at,
            updated_at: self.updated_at,
            closed_at: self.closed_at,
        }
    }

    /// Whether the comments connection has more pages.
    pub fn has_more_comments(&self) -> bool {
        self.comments.page_info.has_next_page
    }

    /// Cursor for fetching the next page of comments.
    pub fn comments_cursor(&self) -> Option<&str> {
        self.comments.page_info.end_cursor.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Pull request response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ListPullRequestsData {
    pub repository: Option<ListPullRequestsRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListPullRequestsRepo {
    pub pull_requests: Connection<GqlPullRequest>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetPullRequestData {
    pub repository: Option<GetPullRequestRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetPullRequestRepo {
    pub pull_request: Option<GqlPullRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlPullRequest {
    pub number: u64,
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub is_draft: bool,
    pub mergeable: String,
    pub author: Option<GqlUser>,
    pub labels: Option<Nodes<GqlLabel>>,
    pub assignees: Option<Nodes<GqlUser>>,
    pub review_decision: Option<String>,
    pub reviews: Connection<GqlReview>,
    pub comments: Connection<GqlComment>,
    pub closing_issues_references: Option<Nodes<GqlClosingIssue>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GqlReview {
    pub id: String,
    pub author: Option<GqlUser>,
    pub state: String,
    pub body: Option<String>,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GqlClosingIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub id: String,
}

impl GqlPullRequest {
    /// Convert to the normalized model type. Requires owner/repo context.
    pub fn into_model(self, owner: &str, repo: &str) -> model::PullRequest {
        let state = match self.state.as_str() {
            "CLOSED" => model::PullRequestState::Closed,
            "MERGED" => model::PullRequestState::Merged,
            _ => model::PullRequestState::Open,
        };

        let mergeable = match self.mergeable.as_str() {
            "MERGEABLE" => Some(model::MergeableState::Mergeable),
            "CONFLICTING" => Some(model::MergeableState::Conflicting),
            "UNKNOWN" => Some(model::MergeableState::Unknown),
            _ => None,
        };

        let review_decision = self.review_decision.as_deref().and_then(|r| match r {
            "APPROVED" => Some(model::ReviewDecision::Approved),
            "CHANGES_REQUESTED" => Some(model::ReviewDecision::ChangesRequested),
            "REVIEW_REQUIRED" => Some(model::ReviewDecision::ReviewRequired),
            _ => None,
        });

        let labels = self
            .labels
            .map(|n| n.nodes.into_iter().map(GqlLabel::into_model).collect())
            .unwrap_or_default();

        let assignees = self
            .assignees
            .map(|n| n.nodes.into_iter().map(GqlUser::into_model).collect())
            .unwrap_or_default();

        let reviews = self
            .reviews
            .nodes
            .into_iter()
            .map(|r| {
                let state = match r.state.as_str() {
                    "APPROVED" => model::ReviewState::Approved,
                    "CHANGES_REQUESTED" => model::ReviewState::ChangesRequested,
                    "DISMISSED" => model::ReviewState::Dismissed,
                    _ => model::ReviewState::Commented,
                };
                model::Review {
                    id: r.id,
                    author: r
                        .author
                        .map(GqlUser::into_model)
                        .unwrap_or_else(ghost_user),
                    state,
                    body: r.body,
                    submitted_at: r.submitted_at,
                }
            })
            .collect();

        let comments = self
            .comments
            .nodes
            .into_iter()
            .map(GqlComment::into_model)
            .collect();

        let linked_issues = self
            .closing_issues_references
            .map(|n| {
                n.nodes
                    .into_iter()
                    .map(|ci| model::LinkedIssueRef {
                        number: ci.number,
                        title: ci.title,
                        state: parse_issue_state(&ci.state),
                        node_id: ci.id,
                    })
                    .collect()
            })
            .unwrap_or_default();

        model::PullRequest {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: self.number,
            node_id: self.id,
            title: self.title,
            body: self.body,
            state,
            head_ref: self.head_ref_name,
            head_sha: self.head_ref_oid,
            base_ref: self.base_ref_name,
            is_draft: self.is_draft,
            mergeable,
            labels,
            assignees,
            review_decision,
            reviews,
            comments,
            linked_issues,
            author: self
                .author
                .map(GqlUser::into_model)
                .unwrap_or_else(ghost_user),
            created_at: self.created_at,
            updated_at: self.updated_at,
            closed_at: self.closed_at,
            merged_at: self.merged_at,
        }
    }

    /// Whether the comments connection has more pages.
    pub fn has_more_comments(&self) -> bool {
        self.comments.page_info.has_next_page
    }

    /// Cursor for fetching the next page of comments.
    pub fn comments_cursor(&self) -> Option<&str> {
        self.comments.page_info.end_cursor.as_deref()
    }

    /// Whether the reviews connection has more pages.
    pub fn has_more_reviews(&self) -> bool {
        self.reviews.page_info.has_next_page
    }

    /// Cursor for fetching the next page of reviews.
    pub fn reviews_cursor(&self) -> Option<&str> {
        self.reviews.page_info.end_cursor.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Comment page response (shared for issue and PR comment pagination)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct IssueCommentsData {
    pub repository: Option<IssueCommentsRepo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueCommentsRepo {
    pub issue: Option<IssueCommentsNode>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueCommentsNode {
    pub comments: Connection<GqlComment>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrCommentsData {
    pub repository: Option<PrCommentsRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrCommentsRepo {
    pub pull_request: Option<PrCommentsNode>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrCommentsNode {
    pub comments: Connection<GqlComment>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrReviewsData {
    pub repository: Option<PrReviewsRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrReviewsRepo {
    pub pull_request: Option<PrReviewsNode>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrReviewsNode {
    pub reviews: Connection<GqlReview>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_issue_state(s: &str) -> model::IssueState {
    match s {
        "CLOSED" => model::IssueState::Closed,
        _ => model::IssueState::Open,
    }
}

fn ghost_user() -> model::User {
    model::User {
        login: "ghost".to_string(),
        node_id: String::new(),
    }
}

/// Extract linked PRs and blocking relationships from timeline events.
///
/// Returns (linked_pull_requests, blocked_by).
///
/// For blocking relationships, we track MARKED_AS_BLOCKED_BY and UNMARKED_AS_BLOCKED_BY
/// events to compute the current blocking state. An issue is blocked if it has been
/// marked as blocked and not subsequently unmarked.
fn extract_timeline_relationships(
    items: Vec<GqlTimelineItem>,
) -> (Vec<model::LinkedPR>, Vec<model::BlockingIssueRef>) {
    let mut pr_seen = std::collections::HashSet::new();
    let mut linked_prs = Vec::new();

    // Track blocking state: true = currently blocking, false = was unblocked.
    // We process events in order and update state accordingly.
    let mut blocking_state: std::collections::HashMap<u64, (GqlBlockingIssue, bool)> =
        std::collections::HashMap::new();

    for item in items {
        // Handle cross-reference events (linked PRs).
        if let Some(source) = item.source {
            // All four fields must be present for this to be a valid PR reference.
            if let (Some(number), Some(title), Some(state), Some(id)) =
                (source.number, source.title, source.state, source.id)
            {
                if pr_seen.insert(number) {
                    let pr_state = match state.as_str() {
                        "CLOSED" => model::PullRequestState::Closed,
                        "MERGED" => model::PullRequestState::Merged,
                        _ => model::PullRequestState::Open,
                    };
                    linked_prs.push(model::LinkedPR {
                        number,
                        title,
                        state: pr_state,
                        node_id: id,
                    });
                }
            }
        }

        // Handle blocking events.
        // Timeline items include both marked and unmarked events.
        // The presence of blocking_issue indicates a blocking-related event.
        // We determine if it's a "mark" or "unmark" by checking if this issue
        // was previously seen. GitHub returns events in chronological order,
        // so the last event for each blocking issue determines its current state.
        if let Some(blocking) = item.blocking_issue {
            let number = blocking.number;
            // Check if this is a removal (unmarked) by seeing if we already have it marked.
            // This is a simplified heuristic - the actual event type would be better,
            // but since we can't distinguish mark/unmark from the struct alone,
            // we track it by order of appearance. First appearance = marked,
            // second appearance of same issue = unmarked, etc.
            blocking_state
                .entry(number)
                .and_modify(|(_, is_blocked)| *is_blocked = !*is_blocked)
                .or_insert((blocking, true));
        }
    }

    // Collect only the issues that are currently blocking.
    let blocked_by: Vec<model::BlockingIssueRef> = blocking_state
        .into_values()
        .filter_map(|(issue, is_blocked)| {
            if is_blocked {
                Some(model::BlockingIssueRef {
                    number: issue.number,
                    title: issue.title,
                    state: parse_issue_state(&issue.state),
                    node_id: issue.id,
                })
            } else {
                None
            }
        })
        .collect();

    (linked_prs, blocked_by)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_issue_response() {
        let json = serde_json::json!({
            "data": {
                "repository": {
                    "issues": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "number": 1,
                            "id": "I_abc",
                            "title": "Test issue",
                            "body": "Some body",
                            "state": "OPEN",
                            "stateReason": null,
                            "author": { "login": "alice", "id": "U_alice" },
                            "labels": { "nodes": [{ "name": "bug", "color": "d73a4a" }] },
                            "assignees": { "nodes": [] },
                            "milestone": null,
                            "comments": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [{
                                    "id": "IC_1",
                                    "author": { "login": "bob", "id": "U_bob" },
                                    "body": "A comment",
                                    "createdAt": "2025-01-01T00:00:00Z",
                                    "updatedAt": "2025-01-01T00:00:00Z"
                                }]
                            },
                            "parent": null,
                            "subIssues": { "nodes": [] },
                            "timelineItems": { "nodes": [] },
                            "createdAt": "2025-01-01T00:00:00Z",
                            "updatedAt": "2025-01-02T00:00:00Z",
                            "closedAt": null
                        }]
                    }
                }
            }
        });

        let resp: GraphQLResponse<ListIssuesData> = serde_json::from_value(json).unwrap();
        let data = resp.data.unwrap();
        let repo = data.repository.unwrap();
        assert_eq!(repo.issues.nodes.len(), 1);

        let issue = repo.issues.nodes.into_iter().next().unwrap();
        let model = issue.into_model("owner", "repo");

        assert_eq!(model.number, 1);
        assert_eq!(model.title, "Test issue");
        assert_eq!(model.state, model::IssueState::Open);
        assert_eq!(model.labels.len(), 1);
        assert_eq!(model.labels[0].name, "bug");
        assert_eq!(model.comments.len(), 1);
        assert_eq!(model.author.login, "alice");
        assert!(model.parent.is_none());
        assert!(model.blocked_by.is_empty());
    }

    #[test]
    fn deserialize_pr_response() {
        let json = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "number": 42,
                            "id": "PR_abc",
                            "title": "Fix bug",
                            "body": "Fixes #1",
                            "state": "OPEN",
                            "headRefName": "fix-bug",
                            "headRefOid": "abc123",
                            "baseRefName": "main",
                            "isDraft": false,
                            "mergeable": "MERGEABLE",
                            "author": { "login": "alice", "id": "U_alice" },
                            "labels": { "nodes": [] },
                            "assignees": { "nodes": [] },
                            "reviewDecision": "APPROVED",
                            "reviews": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [{
                                    "id": "R_1",
                                    "author": { "login": "bob", "id": "U_bob" },
                                    "state": "APPROVED",
                                    "body": "LGTM",
                                    "submittedAt": "2025-01-02T00:00:00Z"
                                }]
                            },
                            "comments": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": []
                            },
                            "closingIssuesReferences": {
                                "nodes": [{
                                    "number": 1,
                                    "title": "Test issue",
                                    "state": "OPEN",
                                    "id": "I_abc"
                                }]
                            },
                            "createdAt": "2025-01-01T00:00:00Z",
                            "updatedAt": "2025-01-02T00:00:00Z",
                            "closedAt": null,
                            "mergedAt": null
                        }]
                    }
                }
            }
        });

        let resp: GraphQLResponse<ListPullRequestsData> = serde_json::from_value(json).unwrap();
        let data = resp.data.unwrap();
        let repo = data.repository.unwrap();
        assert_eq!(repo.pull_requests.nodes.len(), 1);

        let pr = repo.pull_requests.nodes.into_iter().next().unwrap();
        let model = pr.into_model("owner", "repo");

        assert_eq!(model.number, 42);
        assert_eq!(model.state, model::PullRequestState::Open);
        assert_eq!(model.head_ref, "fix-bug");
        assert_eq!(model.head_sha, "abc123");
        assert_eq!(model.mergeable, Some(model::MergeableState::Mergeable));
        assert_eq!(model.review_decision, Some(model::ReviewDecision::Approved));
        assert_eq!(model.reviews.len(), 1);
        assert_eq!(model.linked_issues.len(), 1);
        assert_eq!(model.linked_issues[0].number, 1);
    }

    #[test]
    fn deserialize_issue_with_nulls() {
        let json = serde_json::json!({
            "data": {
                "repository": {
                    "issue": {
                        "number": 5,
                        "id": "I_5",
                        "title": "Minimal",
                        "body": null,
                        "state": "CLOSED",
                        "stateReason": "NOT_PLANNED",
                        "author": null,
                        "labels": null,
                        "assignees": null,
                        "milestone": null,
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        },
                        "parent": null,
                        "subIssues": null,
                        "timelineItems": null,
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2025-01-01T00:00:00Z",
                        "closedAt": "2025-01-02T00:00:00Z"
                    }
                }
            }
        });

        let resp: GraphQLResponse<GetIssueData> = serde_json::from_value(json).unwrap();
        let data = resp.data.unwrap();
        let repo = data.repository.unwrap();
        let issue = repo.issue.unwrap();
        let model = issue.into_model("o", "r");

        assert_eq!(model.state, model::IssueState::Closed);
        assert_eq!(model.state_reason, Some(model::IssueStateReason::NotPlanned));
        assert_eq!(model.author.login, "ghost");
        assert!(model.body.is_none());
        assert!(model.labels.is_empty());
        assert!(model.assignees.is_empty());
        assert!(model.parent.is_none());
        assert!(model.sub_issues.is_empty());
        assert!(model.blocked_by.is_empty());
        assert!(model.linked_pull_requests.is_empty());
        assert!(model.closed_at.is_some());
    }

    #[test]
    fn deserialize_pr_merged() {
        let json = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "number": 10,
                        "id": "PR_10",
                        "title": "Merged PR",
                        "body": null,
                        "state": "MERGED",
                        "headRefName": "feature",
                        "headRefOid": "def456",
                        "baseRefName": "main",
                        "isDraft": false,
                        "mergeable": "UNKNOWN",
                        "author": { "login": "carol", "id": "U_carol" },
                        "labels": { "nodes": [] },
                        "assignees": { "nodes": [] },
                        "reviewDecision": null,
                        "reviews": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        },
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        },
                        "closingIssuesReferences": { "nodes": [] },
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2025-01-03T00:00:00Z",
                        "closedAt": "2025-01-03T00:00:00Z",
                        "mergedAt": "2025-01-03T00:00:00Z"
                    }
                }
            }
        });

        let resp: GraphQLResponse<GetPullRequestData> = serde_json::from_value(json).unwrap();
        let data = resp.data.unwrap();
        let pr = data.repository.unwrap().pull_request.unwrap();
        let model = pr.into_model("o", "r");

        assert_eq!(model.state, model::PullRequestState::Merged);
        assert!(model.merged_at.is_some());
        assert_eq!(model.mergeable, Some(model::MergeableState::Unknown));
    }

    #[test]
    fn linked_prs_from_timeline_deduplicates() {
        let items = vec![
            GqlTimelineItem {
                source: Some(GqlTimelineSource {
                    number: Some(1),
                    title: Some("PR one".into()),
                    state: Some("OPEN".into()),
                    id: Some("PR_1".into()),
                }),
                blocking_issue: None,
            },
            // Duplicate reference to the same PR.
            GqlTimelineItem {
                source: Some(GqlTimelineSource {
                    number: Some(1),
                    title: Some("PR one".into()),
                    state: Some("OPEN".into()),
                    id: Some("PR_1".into()),
                }),
                blocking_issue: None,
            },
            GqlTimelineItem {
                source: Some(GqlTimelineSource {
                    number: Some(2),
                    title: Some("PR two".into()),
                    state: Some("MERGED".into()),
                    id: Some("PR_2".into()),
                }),
                blocking_issue: None,
            },
            // Incomplete source (not a PR) — should be skipped.
            GqlTimelineItem {
                source: Some(GqlTimelineSource {
                    number: None,
                    title: None,
                    state: None,
                    id: None,
                }),
                blocking_issue: None,
            },
        ];

        let (prs, blocked_by) = extract_timeline_relationships(items);
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[1].number, 2);
        assert_eq!(prs[1].state, model::PullRequestState::Merged);
        assert!(blocked_by.is_empty());
    }

    #[test]
    fn blocking_relationships_from_timeline() {
        let items = vec![
            // Issue 10 blocks this issue (marked).
            GqlTimelineItem {
                source: None,
                blocking_issue: Some(GqlBlockingIssue {
                    number: 10,
                    title: "Blocker issue".into(),
                    state: "OPEN".into(),
                    id: "I_10".into(),
                }),
            },
            // Issue 20 blocks this issue (marked).
            GqlTimelineItem {
                source: None,
                blocking_issue: Some(GqlBlockingIssue {
                    number: 20,
                    title: "Another blocker".into(),
                    state: "CLOSED".into(),
                    id: "I_20".into(),
                }),
            },
            // Issue 10 unblocked (unmarked) — toggles the blocked state.
            GqlTimelineItem {
                source: None,
                blocking_issue: Some(GqlBlockingIssue {
                    number: 10,
                    title: "Blocker issue".into(),
                    state: "OPEN".into(),
                    id: "I_10".into(),
                }),
            },
        ];

        let (prs, blocked_by) = extract_timeline_relationships(items);
        assert!(prs.is_empty());
        // Only issue 20 should remain as a blocker (issue 10 was unmarked).
        assert_eq!(blocked_by.len(), 1);
        assert_eq!(blocked_by[0].number, 20);
        assert_eq!(blocked_by[0].state, model::IssueState::Closed);
    }

    #[test]
    fn deserialize_issue_with_parent_and_blockers() {
        let json = serde_json::json!({
            "data": {
                "repository": {
                    "issue": {
                        "number": 42,
                        "id": "I_42",
                        "title": "Sub-task",
                        "body": "A sub-issue",
                        "state": "OPEN",
                        "stateReason": null,
                        "author": { "login": "dev", "id": "U_dev" },
                        "labels": { "nodes": [] },
                        "assignees": { "nodes": [] },
                        "milestone": null,
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        },
                        "parent": {
                            "number": 10,
                            "title": "Epic issue",
                            "state": "OPEN",
                            "id": "I_10"
                        },
                        "subIssues": { "nodes": [] },
                        "timelineItems": {
                            "nodes": [{
                                "blockingIssue": {
                                    "number": 5,
                                    "title": "Must fix first",
                                    "state": "OPEN",
                                    "id": "I_5"
                                }
                            }]
                        },
                        "createdAt": "2025-01-01T00:00:00Z",
                        "updatedAt": "2025-01-02T00:00:00Z",
                        "closedAt": null
                    }
                }
            }
        });

        let resp: GraphQLResponse<GetIssueData> = serde_json::from_value(json).unwrap();
        let data = resp.data.unwrap();
        let issue = data.repository.unwrap().issue.unwrap();
        let model = issue.into_model("owner", "repo");

        assert_eq!(model.number, 42);

        // Check parent is populated.
        let parent = model.parent.unwrap();
        assert_eq!(parent.number, 10);
        assert_eq!(parent.title, "Epic issue");
        assert_eq!(parent.state, model::IssueState::Open);

        // Check blocked_by is populated.
        assert_eq!(model.blocked_by.len(), 1);
        assert_eq!(model.blocked_by[0].number, 5);
        assert_eq!(model.blocked_by[0].title, "Must fix first");
    }

    #[test]
    fn graphql_errors_deserialize() {
        let json = serde_json::json!({
            "data": null,
            "errors": [{
                "message": "Field 'subIssues' doesn't exist",
                "type": "FIELD_ERROR",
                "path": ["repository", "issues", "nodes", 0, "subIssues"],
                "locations": [{ "line": 10, "column": 5 }]
            }]
        });

        let resp: GraphQLResponse<ListIssuesData> = serde_json::from_value(json).unwrap();
        assert!(resp.data.is_none());
        let errors = resp.errors.unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("subIssues"));
        assert_eq!(errors[0].error_type.as_deref(), Some("FIELD_ERROR"));
    }
}
