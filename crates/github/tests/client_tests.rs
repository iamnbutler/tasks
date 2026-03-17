//! Mock server tests for GitHubClient — spec github.md §6.3.

use chrono::{DateTime, Utc};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tasks_github::client::GitHubClient;
use tasks_github::error::GitHubError;
use tasks_github::model::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mock_client(base_url: &str) -> GitHubClient {
    GitHubClient::builder("test-token")
        .base_url(base_url)
        .rate_limit_floor(0) // disable rate limit pausing in tests
        .build()
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

fn single_issue_response(issue: serde_json::Value) -> serde_json::Value {
    json!({
        "data": {
            "repository": {
                "issue": issue
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

fn single_pr_response(pr: serde_json::Value) -> serde_json::Value {
    json!({
        "data": {
            "repository": {
                "pullRequest": pr
            }
        }
    })
}

fn sample_issue() -> serde_json::Value {
    json!({
        "number": 1,
        "id": "I_abc",
        "title": "Test issue",
        "body": "Body text",
        "state": "OPEN",
        "stateReason": null,
        "author": { "login": "alice", "id": "U_alice" },
        "labels": { "nodes": [{ "name": "bug", "color": "d73a4a" }] },
        "assignees": { "nodes": [{ "login": "bob", "id": "U_bob" }] },
        "milestone": { "title": "v1.0", "number": 1, "state": "OPEN" },
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
        "subIssues": { "nodes": [
            { "number": 2, "title": "Sub-issue", "state": "OPEN", "id": "I_sub" }
        ]},
        "timelineItems": { "nodes": [
            { "source": { "number": 10, "title": "Fix PR", "state": "OPEN", "id": "PR_10" } }
        ]},
        "createdAt": "2025-01-01T00:00:00Z",
        "updatedAt": "2025-01-02T00:00:00Z",
        "closedAt": null
    })
}

fn sample_pr() -> serde_json::Value {
    json!({
        "number": 10,
        "id": "PR_10",
        "title": "Fix bug",
        "body": "Fixes #1",
        "state": "OPEN",
        "headRefName": "fix-bug",
        "headRefOid": "abc123def456",
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
            "nodes": [{ "number": 1, "title": "Test issue", "state": "OPEN", "id": "I_abc" }]
        },
        "createdAt": "2025-01-01T00:00:00Z",
        "updatedAt": "2025-01-02T00:00:00Z",
        "closedAt": null,
        "mergedAt": null
    })
}

// ---------------------------------------------------------------------------
// Issue tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_issues_returns_normalized_issues() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_response(json!([sample_issue()]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let issues = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await
        .unwrap();

    assert_eq!(issues.len(), 1);
    let issue = &issues[0];
    assert_eq!(issue.owner, "owner");
    assert_eq!(issue.repo, "repo");
    assert_eq!(issue.number, 1);
    assert_eq!(issue.node_id, "I_abc");
    assert_eq!(issue.title, "Test issue");
    assert_eq!(issue.body.as_deref(), Some("Body text"));
    assert_eq!(issue.state, IssueState::Open);
    assert_eq!(issue.author.login, "alice");
    assert_eq!(issue.labels.len(), 1);
    assert_eq!(issue.labels[0].name, "bug");
    assert_eq!(issue.assignees.len(), 1);
    assert_eq!(issue.assignees[0].login, "bob");
    assert!(issue.milestone.is_some());
    assert_eq!(issue.milestone.as_ref().unwrap().title, "v1.0");
    assert_eq!(issue.comments.len(), 1);
    assert_eq!(issue.sub_issues.len(), 1);
    assert_eq!(issue.sub_issues[0].number, 2);
    assert_eq!(issue.linked_pull_requests.len(), 1);
    assert_eq!(issue.linked_pull_requests[0].number, 10);
}

#[tokio::test]
async fn get_issue_returns_single_issue() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("GetIssue"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(single_issue_response(sample_issue())),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let issue = client.get_issue("owner", "repo", 1).await.unwrap();

    assert_eq!(issue.number, 1);
    assert_eq!(issue.title, "Test issue");
}

#[tokio::test]
async fn list_issues_with_filters() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("OPEN"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_response(json!([sample_issue()]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let filters = IssueFilters {
        states: Some(vec![IssueState::Open]),
        labels: Some(vec!["bug".to_string()]),
        since: Some("2025-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
    };

    let issues = client.list_issues("owner", "repo", &filters).await.unwrap();
    assert_eq!(issues.len(), 1);
}

// ---------------------------------------------------------------------------
// Pull request tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_pull_requests_returns_normalized_prs() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(pr_response(json!([sample_pr()]))),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let prs = client
        .list_pull_requests("owner", "repo", &PullRequestFilters::default())
        .await
        .unwrap();

    assert_eq!(prs.len(), 1);
    let pr = &prs[0];
    assert_eq!(pr.owner, "owner");
    assert_eq!(pr.repo, "repo");
    assert_eq!(pr.number, 10);
    assert_eq!(pr.title, "Fix bug");
    assert_eq!(pr.state, PullRequestState::Open);
    assert_eq!(pr.head_ref, "fix-bug");
    assert_eq!(pr.head_sha, "abc123def456");
    assert_eq!(pr.base_ref, "main");
    assert!(!pr.is_draft);
    assert_eq!(pr.mergeable, Some(MergeableState::Mergeable));
    assert_eq!(pr.review_decision, Some(ReviewDecision::Approved));
    assert_eq!(pr.reviews.len(), 1);
    assert_eq!(pr.reviews[0].state, ReviewState::Approved);
    assert_eq!(pr.linked_issues.len(), 1);
    assert_eq!(pr.linked_issues[0].number, 1);
}

#[tokio::test]
async fn get_pull_request_returns_single_pr() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("GetPullRequest"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(single_pr_response(sample_pr())),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let pr = client.get_pull_request("owner", "repo", 10).await.unwrap();

    assert_eq!(pr.number, 10);
    assert_eq!(pr.title, "Fix bug");
}

// ---------------------------------------------------------------------------
// Pagination tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_issues_paginates() {
    let server = MockServer::start().await;

    // Page 1: has next page.
    let page1 = json!({
        "data": {
            "repository": {
                "issues": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "cursor1" },
                    "nodes": [sample_issue()]
                }
            }
        }
    });

    // Page 2: no more pages.
    let mut issue2 = sample_issue();
    issue2["number"] = json!(2);
    issue2["id"] = json!("I_def");
    issue2["title"] = json!("Second issue");
    let page2 = json!({
        "data": {
            "repository": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [issue2]
                }
            }
        }
    });

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page1))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("cursor1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page2))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let issues = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await
        .unwrap();

    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].number, 1);
    assert_eq!(issues[1].number, 2);
}

#[tokio::test]
async fn max_pages_stops_pagination() {
    let server = MockServer::start().await;

    // Always return has_next_page=true.
    let page = json!({
        "data": {
            "repository": {
                "issues": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "cursor" },
                    "nodes": [sample_issue()]
                }
            }
        }
    });

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .expect(2)
        .mount(&server)
        .await;

    let client = GitHubClient::builder("test-token")
        .base_url(server.uri())
        .max_pages(2)
        .rate_limit_floor(0)
        .build();

    let issues = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await
        .unwrap();

    // 2 pages * 1 issue each = 2 issues.
    assert_eq!(issues.len(), 2);
}

// ---------------------------------------------------------------------------
// Comment pagination tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetches_additional_comment_pages() {
    let server = MockServer::start().await;

    // Issue with has_next_page=true on comments.
    let mut issue = sample_issue();
    issue["comments"]["pageInfo"]["hasNextPage"] = json!(true);
    issue["comments"]["pageInfo"]["endCursor"] = json!("comment_cursor");

    // First request: list issues.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListIssues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_response(json!([issue]))),
        )
        .mount(&server)
        .await;

    // Second request: fetch additional comments.
    let comment_page = json!({
        "data": {
            "repository": {
                "issue": {
                    "comments": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "id": "IC_2",
                            "author": { "login": "carol", "id": "U_carol" },
                            "body": "Another comment",
                            "createdAt": "2025-01-02T00:00:00Z",
                            "updatedAt": "2025-01-02T00:00:00Z"
                        }]
                    }
                }
            }
        }
    });

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("IssueComments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(comment_page))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let issues = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await
        .unwrap();

    assert_eq!(issues.len(), 1);
    // Original comment + fetched comment = 2.
    assert_eq!(issues[0].comments.len(), 2);
    assert_eq!(issues[0].comments[0].id, "IC_1");
    assert_eq!(issues[0].comments[1].id, "IC_2");
}

// ---------------------------------------------------------------------------
// Error handling tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_error_on_401() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await;

    assert!(matches!(result, Err(GitHubError::Auth(_))));
}

#[tokio::test]
async fn not_found_on_missing_repo() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": null },
            "errors": [{
                "message": "Could not resolve to a Repository",
                "type": "NOT_FOUND",
                "path": ["repository"]
            }]
        })))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client
        .list_issues("owner", "nonexistent", &IssueFilters::default())
        .await;

    assert!(matches!(result, Err(GitHubError::NotFound(_))));
}

#[tokio::test]
async fn graphql_errors_surfaced() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{
                "message": "Field 'badField' doesn't exist",
                "type": "FIELD_ERROR"
            }]
        })))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await;

    assert!(matches!(result, Err(GitHubError::GraphQL(_))));
}

#[tokio::test]
async fn rate_limit_headers_parsed() {
    let server = MockServer::start().await;

    let reset_ts = (Utc::now() + chrono::Duration::hours(1)).timestamp();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([])))
                .insert_header("x-ratelimit-remaining", "4500")
                .insert_header("x-ratelimit-reset", &reset_ts.to_string()),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let _ = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await
        .unwrap();

    let rl = client.rate_limit().unwrap();
    assert_eq!(rl.remaining, 4500);
}

#[tokio::test]
async fn rate_limited_403_retries_once() {
    let server = MockServer::start().await;

    let reset_ts = (Utc::now() - chrono::Duration::seconds(1)).timestamp();

    // First call: 403 with rate limit exhausted.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string("rate limit exceeded")
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", &reset_ts.to_string()),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Second call (retry): success.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_response(json!([])))
                .insert_header("x-ratelimit-remaining", "4999")
                .insert_header("x-ratelimit-reset", &reset_ts.to_string()),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let issues = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await
        .unwrap();

    assert!(issues.is_empty());
}

#[tokio::test]
async fn decode_error_on_garbage_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client
        .list_issues("owner", "repo", &IssueFilters::default())
        .await;

    assert!(matches!(result, Err(GitHubError::Decode(_))));
}

// ---------------------------------------------------------------------------
// PR since filter (client-side cutoff)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pr_since_filter_stops_at_cutoff() {
    let server = MockServer::start().await;

    let mut old_pr = sample_pr();
    old_pr["number"] = json!(11);
    old_pr["id"] = json!("PR_11");
    old_pr["title"] = json!("Old PR");
    old_pr["updatedAt"] = json!("2024-01-01T00:00:00Z");

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr_response(json!([
            sample_pr(), // updated 2025-01-02
            old_pr       // updated 2024-01-01
        ]))))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let filters = PullRequestFilters {
        since: Some("2025-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
        ..Default::default()
    };

    let prs = client
        .list_pull_requests("owner", "repo", &filters)
        .await
        .unwrap();

    // Only the recent PR should be included.
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 10);
}

// ---------------------------------------------------------------------------
// File content tests (spec §14 — workflow config loading)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_file_content_returns_raw_content() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/contents/workflow.toml"))
        .and(header("accept", "application/vnd.github.raw+json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("[prompt]\nsystem_prompt = \"prompt.md\""),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let content = client
        .get_file_content("owner", "repo", "workflow.toml")
        .await
        .unwrap();

    assert!(content.is_some());
    assert!(content.as_ref().unwrap().contains("system_prompt"));
}

#[tokio::test]
async fn get_file_content_returns_none_for_404() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/contents/nonexistent.toml"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not found"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let content = client
        .get_file_content("owner", "repo", "nonexistent.toml")
        .await
        .unwrap();

    assert!(content.is_none());
}

#[tokio::test]
async fn get_file_content_auth_error_on_401() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/contents/workflow.toml"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client
        .get_file_content("owner", "repo", "workflow.toml")
        .await;

    assert!(matches!(result, Err(GitHubError::Auth(_))));
}

#[tokio::test]
async fn get_file_content_fetches_nested_path() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/contents/.tasks/prompt.md"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("# System Prompt\n\nUse conventional commits."),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let content = client
        .get_file_content("owner", "repo", ".tasks/prompt.md")
        .await
        .unwrap();

    assert!(content.is_some());
    assert!(content.as_ref().unwrap().contains("conventional commits"));
}

// ---------------------------------------------------------------------------
// Branch deletion tests (issue #143 — cleanup on rejection)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_branch_returns_true_on_success() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/repos/owner/repo/git/refs/heads/tasks/test-task-123"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let deleted = client
        .delete_branch("owner", "repo", "tasks/test-task-123")
        .await
        .unwrap();

    assert!(deleted);
}

#[tokio::test]
async fn delete_branch_returns_false_on_404() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/repos/owner/repo/git/refs/heads/nonexistent-branch"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Reference does not exist"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let deleted = client
        .delete_branch("owner", "repo", "nonexistent-branch")
        .await
        .unwrap();

    // 404 is not an error — the branch simply doesn't exist
    assert!(!deleted);
}

#[tokio::test]
async fn delete_branch_auth_error_on_401() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/repos/owner/repo/git/refs/heads/tasks/test-task"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client
        .delete_branch("owner", "repo", "tasks/test-task")
        .await;

    assert!(matches!(result, Err(GitHubError::Auth(_))));
}

#[tokio::test]
async fn delete_branch_auth_error_on_protected_branch() {
    let server = MockServer::start().await;

    // 403 for protected branches (not rate limiting)
    Mock::given(method("DELETE"))
        .and(path("/repos/owner/repo/git/refs/heads/main"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string("Cannot delete protected branch")
                .insert_header("x-ratelimit-remaining", "4999"),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client.delete_branch("owner", "repo", "main").await;

    // 403 with remaining rate limit = auth error (protected branch)
    assert!(matches!(result, Err(GitHubError::Auth(_))));
}

// ---------------------------------------------------------------------------
// File content at ref tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_file_content_at_ref_sends_ref_param() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/contents/src/main.rs"))
        .and(query_param("ref", "feature-branch"))
        .and(header("accept", "application/vnd.github.raw+json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("fn main() {}"),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let content = client
        .get_file_content_at_ref("owner", "repo", "src/main.rs", "feature-branch")
        .await
        .unwrap();

    assert_eq!(content, Some("fn main() {}".to_string()));
}

// ---------------------------------------------------------------------------
// PR diff tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_pr_diff_returns_unified_diff() {
    let server = MockServer::start().await;

    let diff_body = "\
diff --git a/src/main.rs b/src/main.rs
index abc1234..def5678 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    println!(\"added\");
 }
";

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/10"))
        .and(header("accept", "application/vnd.github.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diff_body))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let diff = client.get_pr_diff("owner", "repo", 10).await.unwrap();
    assert!(diff.contains("@@"));
    assert!(diff.contains("+    println!(\"new\")"));
}

#[tokio::test]
async fn get_pr_diff_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/999"))
        .and(header("accept", "application/vnd.github.diff"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client.get_pr_diff("owner", "repo", 999).await;
    assert!(matches!(result, Err(GitHubError::NotFound(_))));
}

#[tokio::test]
async fn get_pr_diff_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/10"))
        .and(header("accept", "application/vnd.github.diff"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let result = client.get_pr_diff("owner", "repo", 10).await;
    assert!(matches!(result, Err(GitHubError::Auth(_))));
}

#[tokio::test]
async fn get_pr_diff_truncates_large_diffs() {
    let server = MockServer::start().await;

    // Generate a diff larger than 100KB
    let large_diff = "a".repeat(150_000);

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/10"))
        .and(header("accept", "application/vnd.github.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(&large_diff))
        .mount(&server)
        .await;

    let client = mock_client(&server.uri());
    let diff = client.get_pr_diff("owner", "repo", 10).await.unwrap();
    // Should be truncated to MAX_DIFF_SIZE + truncation notice
    assert!(diff.len() < 150_000);
    assert!(diff.contains("[diff truncated"));
}
