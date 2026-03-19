//! Mock server tests for RepoPoller — spec github.md §6.3.

use chrono::{DateTime, Utc};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tasks_github::client::GitHubClient;
use tasks_github::RepoPoller;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mock_client(base_url: &str) -> GitHubClient {
    GitHubClient::builder("test-token")
        .base_url(base_url)
        .rate_limit_floor(0)
        .build()
}

fn sample_issue(number: u64, updated_at: &str) -> serde_json::Value {
    json!({
        "number": number,
        "id": format!("I_{number}"),
        "title": format!("Issue {number}"),
        "body": null,
        "state": "OPEN",
        "stateReason": null,
        "author": { "login": "alice", "id": "U_alice" },
        "labels": { "nodes": [] },
        "assignees": { "nodes": [] },
        "milestone": null,
        "comments": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": []
        },
        "subIssues": { "nodes": [] },
        "timelineItems": { "nodes": [] },
        "createdAt": "2025-01-01T00:00:00Z",
        "updatedAt": updated_at,
        "closedAt": null
    })
}

fn sample_pr(number: u64, updated_at: &str) -> serde_json::Value {
    json!({
        "number": number,
        "id": format!("PR_{number}"),
        "title": format!("PR {number}"),
        "body": null,
        "state": "OPEN",
        "headRefName": "branch",
        "headRefOid": "abc123",
        "baseRefName": "main",
        "isDraft": false,
        "mergeable": "UNKNOWN",
        "author": { "login": "alice", "id": "U_alice" },
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
        "updatedAt": updated_at,
        "closedAt": null,
        "mergedAt": null
    })
}

fn issue_response(issues: serde_json::Value) -> serde_json::Value {
    json!({
        "data": {
            "repository": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": issues
                }
            }
        }
    })
}

fn pr_response(prs: serde_json::Value) -> serde_json::Value {
    json!({
        "data": {
            "repository": {
                "pullRequests": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": prs
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_poll_fetches_all_open_items() {
    let server = MockServer::start().await;

    // The poller fetches issues and PRs concurrently. Use body matchers to
    // route each request to the correct mock regardless of arrival order.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([
                    sample_issue(1, "2025-01-10T00:00:00Z"),
                    sample_issue(2, "2025-01-11T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPullRequests"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([
                    sample_pr(10, "2025-01-12T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    assert!(poller.since().is_none());

    let result = poller.poll().await.unwrap();

    assert_eq!(result.issues.len(), 2);
    assert_eq!(result.pull_requests.len(), 1);
    assert!(poller.since().is_some());
}

#[tokio::test]
async fn poll_advances_high_water_mark() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([
                    sample_issue(1, "2025-01-10T00:00:00Z"),
                    sample_issue(2, "2025-01-15T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPullRequests"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([
                    sample_pr(10, "2025-01-12T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let result = poller.poll().await.unwrap();

    // High-water mark should be the max updated_at across all items.
    let expected: DateTime<Utc> = "2025-01-15T00:00:00Z".parse().unwrap();
    assert_eq!(result.timestamp, Some(expected));
    assert_eq!(poller.since(), Some(expected));
}

#[tokio::test]
async fn poll_does_not_advance_on_failure() {
    let server = MockServer::start().await;

    // First poll succeeds. up_to_n_times(1) ensures these are consumed so the
    // 401 mock below takes effect for the second poll.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([
                    sample_issue(1, "2025-01-10T00:00:00Z"),
                ]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPullRequests"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let _ = poller.poll().await.unwrap();
    let mark_after_first = poller.since();
    assert!(mark_after_first.is_some());

    // Second poll fails (server returns 401 for all requests).
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let result = poller.poll().await;
    assert!(result.is_err());

    // High-water mark should not have changed.
    assert_eq!(poller.since(), mark_after_first);
}

#[tokio::test]
async fn poll_issues_only() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([
                    sample_issue(1, "2025-01-10T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let issues = poller.poll_issues().await.unwrap();
    assert_eq!(issues.len(), 1);

    let expected: DateTime<Utc> = "2025-01-10T00:00:00Z".parse().unwrap();
    assert_eq!(poller.since(), Some(expected));
}

#[tokio::test]
async fn poll_pull_requests_only() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPullRequests"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([
                    sample_pr(10, "2025-01-12T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let prs = poller.poll_pull_requests().await.unwrap();
    assert_eq!(prs.len(), 1);

    let expected: DateTime<Utc> = "2025-01-12T00:00:00Z".parse().unwrap();
    assert_eq!(poller.since(), Some(expected));
}

#[tokio::test]
async fn empty_poll_does_not_clear_high_water_mark() {
    let server = MockServer::start().await;

    // First poll returns items. up_to_n_times(1) ensures these are consumed
    // before the empty fallback mocks below.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([
                    sample_issue(1, "2025-01-10T00:00:00Z"),
                ]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPullRequests"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let _ = poller.poll().await.unwrap();
    let first_mark = poller.since();
    assert!(first_mark.is_some());

    // Second poll returns nothing (fallback mocks serve empty responses).
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([]))),
        )
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPullRequests"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([]))),
        )
        .mount(&server)
        .await;

    let result = poller.poll().await.unwrap();
    assert!(result.issues.is_empty());
    assert!(result.pull_requests.is_empty());

    // Mark should not have regressed.
    assert_eq!(poller.since(), first_mark);
}

// ---------------------------------------------------------------------------
// Closure detection tests (spec §11.3)
// ---------------------------------------------------------------------------

fn sample_closed_issue(number: u64, updated_at: &str, state_reason: Option<&str>) -> serde_json::Value {
    json!({
        "number": number,
        "id": format!("I_{number}"),
        "title": format!("Issue {number}"),
        "body": null,
        "state": "CLOSED",
        "stateReason": state_reason,
        "author": { "login": "alice", "id": "U_alice" },
        "labels": { "nodes": [] },
        "assignees": { "nodes": [] },
        "milestone": null,
        "comments": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": []
        },
        "subIssues": { "nodes": [] },
        "timelineItems": { "nodes": [] },
        "createdAt": "2025-01-01T00:00:00Z",
        "updatedAt": updated_at,
        "closedAt": updated_at
    })
}

fn sample_merged_pr(number: u64, updated_at: &str) -> serde_json::Value {
    json!({
        "number": number,
        "id": format!("PR_{number}"),
        "title": format!("PR {number}"),
        "body": null,
        "state": "MERGED",
        "headRefName": "branch",
        "headRefOid": "abc123",
        "baseRefName": "main",
        "isDraft": false,
        "mergeable": "UNKNOWN",
        "author": { "login": "alice", "id": "U_alice" },
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
        "updatedAt": updated_at,
        "closedAt": updated_at,
        "mergedAt": updated_at
    })
}

fn sample_closed_pr(number: u64, updated_at: &str) -> serde_json::Value {
    json!({
        "number": number,
        "id": format!("PR_{number}"),
        "title": format!("PR {number}"),
        "body": null,
        "state": "CLOSED",
        "headRefName": "branch",
        "headRefOid": "abc123",
        "baseRefName": "main",
        "isDraft": false,
        "mergeable": "UNKNOWN",
        "author": { "login": "alice", "id": "U_alice" },
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
        "updatedAt": updated_at,
        "closedAt": updated_at,
        "mergedAt": null
    })
}

#[tokio::test]
async fn poll_includes_closed_issues() {
    // Verify that the poller fetches both open and closed issues
    // so we can detect external closures (spec §11.3).
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([
                    sample_issue(1, "2025-01-10T00:00:00Z"),
                    sample_closed_issue(2, "2025-01-11T00:00:00Z", Some("COMPLETED")),
                    sample_closed_issue(3, "2025-01-12T00:00:00Z", Some("NOT_PLANNED")),
                ]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let result = poller.poll().await.unwrap();

    // Should include both open and closed issues
    assert_eq!(result.issues.len(), 3);

    // Verify we can distinguish closed issues
    use tasks_github::model::IssueState;
    let open_issues: Vec<_> = result.issues.iter()
        .filter(|i| i.state == IssueState::Open)
        .collect();
    let closed_issues: Vec<_> = result.issues.iter()
        .filter(|i| i.state == IssueState::Closed)
        .collect();

    assert_eq!(open_issues.len(), 1);
    assert_eq!(closed_issues.len(), 2);
}

#[tokio::test]
async fn poll_includes_merged_prs() {
    // Verify that the poller fetches merged PRs so we can detect
    // external merges (spec §11.3).
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([
                    sample_pr(10, "2025-01-10T00:00:00Z"),
                    sample_merged_pr(11, "2025-01-11T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let result = poller.poll().await.unwrap();

    // Should include both open and merged PRs
    assert_eq!(result.pull_requests.len(), 2);

    // Verify we can distinguish merged PRs
    use tasks_github::model::PullRequestState;
    let open_prs: Vec<_> = result.pull_requests.iter()
        .filter(|p| p.state == PullRequestState::Open)
        .collect();
    let merged_prs: Vec<_> = result.pull_requests.iter()
        .filter(|p| p.state == PullRequestState::Merged)
        .collect();

    assert_eq!(open_prs.len(), 1);
    assert_eq!(merged_prs.len(), 1);
}

#[tokio::test]
async fn poll_includes_closed_prs() {
    // Verify that the poller fetches closed (not merged) PRs so we can
    // detect external closures (spec §11.3).
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([]))),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(pr_response(json!([
                    sample_pr(10, "2025-01-10T00:00:00Z"),
                    sample_closed_pr(11, "2025-01-11T00:00:00Z"),
                ]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let mut poller = RepoPoller::new(client, "owner", "repo");

    let result = poller.poll().await.unwrap();

    // Should include both open and closed PRs
    assert_eq!(result.pull_requests.len(), 2);

    // Verify we can distinguish closed PRs
    use tasks_github::model::PullRequestState;
    let open_prs: Vec<_> = result.pull_requests.iter()
        .filter(|p| p.state == PullRequestState::Open)
        .collect();
    let closed_prs: Vec<_> = result.pull_requests.iter()
        .filter(|p| p.state == PullRequestState::Closed)
        .collect();

    assert_eq!(open_prs.len(), 1);
    assert_eq!(closed_prs.len(), 1);
}
