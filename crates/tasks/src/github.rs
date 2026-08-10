//! GitHub GraphQL client and issue normalization.
//!
//! The client fetches issues from a single repository via GraphQL and
//! returns normalized [`GhIssue`] records. Persistence + task upsert lives
//! on [`crate::store::Store`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use crate::models::GhState;

const DEFAULT_BASE_URL: &str = "https://api.github.com/graphql";
const DEFAULT_REST_BASE_URL: &str = "https://api.github.com";
const PAGE_SIZE: u32 = 100;
const CLOSE_INFO_BATCH: usize = 50;

#[derive(Debug, Error)]
pub enum GhError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("graphql: {0}")]
    GraphQl(String),
    #[error("rest: {0}")]
    Rest(String),
    #[error("unexpected response shape: {0}")]
    Shape(String),
}

/// Normalized GitHub issue, ready for upsert into tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: GhState,
    pub updated_at: DateTime<Utc>,
}

/// How a specific issue looks right now — state plus GitHub's close reason
/// (`stateReason`: COMPLETED, NOT_PLANNED, DUPLICATE, ...). Fetched at
/// decision time and never persisted; GitHub owns these facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCloseInfo {
    pub state: GhState,
    pub state_reason: Option<String>,
}

pub struct GitHubClient {
    http: reqwest::Client,
    token: String,
    base_url: String,
    rest_base_url: String,
}

impl GitHubClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("tasks/0.1")
            .build()
            .expect("reqwest client");
        Self {
            http,
            token: token.into(),
            base_url: base_url.into(),
            rest_base_url: DEFAULT_REST_BASE_URL.into(),
        }
    }

    /// Override the REST root (`GITHUB_REST_API_URL`) — GitHub Enterprise,
    /// tests. Issues stay on GraphQL; only PR creation uses REST.
    pub fn with_rest_base_url(mut self, rest_base_url: impl Into<String>) -> Self {
        self.rest_base_url = rest_base_url.into();
        self
    }

    /// Open a pull request — the system's only GitHub write, and it lives on
    /// the server. Returns the PR *number*: an immutable identifier we may
    /// persist. The PR's state/mergeability/checks are GitHub's and are not
    /// read here, let alone stored.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        name: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<u64, GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls", self.rest_base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "title": title,
                "head": head,
                "base": base,
                "body": body,
            }))
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            let msg = body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("(no message)");
            return Err(GhError::Rest(format!(
                "create pull request: {status}: {msg}"
            )));
        }
        body.get("number")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| GhError::Shape("pull request response missing `number`".into()))
    }

    /// Fetch all OPEN issues for a repository, paging as needed.
    pub async fn list_open_issues(&self, owner: &str, name: &str) -> Result<Vec<GhIssue>, GhError> {
        let mut issues = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (batch, next) = self.fetch_page(owner, name, cursor.as_deref()).await?;
            issues.extend(batch);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        debug!(owner, name, count = issues.len(), "fetched open issues");
        Ok(issues)
    }

    /// Look up state + close reason for specific issue numbers, batched via
    /// GraphQL aliases. Issues that don't resolve (deleted, converted to a
    /// discussion, transferred) are simply absent from the returned map.
    ///
    /// GitHub answers an unresolvable alias with a null node *and* an errors
    /// entry, so unlike [`Self::list_open_issues`] this tolerates errors as
    /// long as `data.repository` came back — otherwise one dead number would
    /// poison the whole batch.
    pub async fn issue_close_info(
        &self,
        owner: &str,
        name: &str,
        numbers: &[u64],
    ) -> Result<std::collections::HashMap<u64, IssueCloseInfo>, GhError> {
        let mut out = std::collections::HashMap::new();
        for chunk in numbers.chunks(CLOSE_INFO_BATCH) {
            let fields: String = chunk
                .iter()
                .map(|n| format!("i{n}: issue(number: {n}) {{ number state stateReason }}\n"))
                .collect();
            let query = format!(
                "query($owner: String!, $name: String!) {{\n\
                   repository(owner: $owner, name: $name) {{\n{fields}}}\n}}"
            );
            let body = serde_json::json!({
                "query": query,
                "variables": { "owner": owner, "name": name },
            });

            let resp: serde_json::Value = self
                .http
                .post(&self.base_url)
                .bearer_auth(&self.token)
                .header("Accept", "application/json")
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let repository = resp.pointer("/data/repository").filter(|v| !v.is_null());
            let Some(repository) = repository else {
                if let Some(errs) = resp.get("errors").and_then(|e| e.as_array()) {
                    let msg = errs
                        .iter()
                        .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(GhError::GraphQl(msg));
                }
                return Err(GhError::Shape("repository is null".into()));
            };
            let nodes = repository
                .as_object()
                .ok_or_else(|| GhError::Shape("repository is not an object".into()))?;

            for (alias, node) in nodes {
                if node.is_null() {
                    warn!(alias, "issue did not resolve; leaving it out");
                    continue;
                }
                let number = node
                    .get("number")
                    .and_then(|n| n.as_u64())
                    .ok_or_else(|| GhError::Shape(format!("{alias}: number missing")))?;
                let state = match node.get("state").and_then(|s| s.as_str()) {
                    Some("OPEN") => GhState::Open,
                    _ => GhState::Closed,
                };
                let state_reason = node
                    .get("stateReason")
                    .and_then(|r| r.as_str())
                    .map(str::to_owned);
                out.insert(
                    number,
                    IssueCloseInfo {
                        state,
                        state_reason,
                    },
                );
            }
        }
        debug!(
            owner,
            name,
            asked = numbers.len(),
            resolved = out.len(),
            "fetched issue close info"
        );
        Ok(out)
    }

    async fn fetch_page(
        &self,
        owner: &str,
        name: &str,
        after: Option<&str>,
    ) -> Result<(Vec<GhIssue>, Option<String>), GhError> {
        let query = r#"
        query($owner: String!, $name: String!, $after: String, $first: Int!) {
          repository(owner: $owner, name: $name) {
            issues(states: [OPEN], first: $first, after: $after, orderBy: { field: UPDATED_AT, direction: DESC }) {
              pageInfo { hasNextPage endCursor }
              nodes {
                number
                title
                body
                state
                updatedAt
                labels(first: 20) { nodes { name } }
              }
            }
          }
        }
        "#;

        let req_body = GraphQlRequest {
            query,
            variables: Variables {
                owner,
                name,
                after,
                first: PAGE_SIZE,
            },
        };

        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .json(&req_body)
            .send()
            .await?
            .error_for_status()?
            .json::<GraphQlResponse>()
            .await?;

        if let Some(errs) = resp.errors {
            let msg = errs
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::GraphQl(msg));
        }

        let data = resp
            .data
            .ok_or_else(|| GhError::Shape("response missing `data` field".into()))?;
        let issues = data
            .repository
            .ok_or_else(|| GhError::Shape("repository is null".into()))?
            .issues;

        let nodes = issues
            .nodes
            .into_iter()
            .map(normalize_issue)
            .collect::<Result<Vec<_>, _>>()?;
        let next = if issues.page_info.has_next_page {
            issues.page_info.end_cursor
        } else {
            None
        };
        Ok((nodes, next))
    }
}

fn normalize_issue(raw: RawIssue) -> Result<GhIssue, GhError> {
    let state = match raw.state.as_str() {
        "OPEN" => GhState::Open,
        "CLOSED" => GhState::Closed,
        other => {
            warn!(state = other, "unknown issue state, treating as open");
            GhState::Open
        }
    };
    let updated_at = DateTime::parse_from_rfc3339(&raw.updated_at)
        .map_err(|e| GhError::Shape(format!("updatedAt parse: {e}")))?
        .with_timezone(&Utc);
    let labels = raw
        .labels
        .map(|l| l.nodes.into_iter().map(|n| n.name).collect())
        .unwrap_or_default();

    Ok(GhIssue {
        number: raw.number,
        title: raw.title,
        body: raw.body.unwrap_or_default(),
        labels,
        state,
        updated_at,
    })
}

// --- wire types ---

#[derive(Serialize)]
struct GraphQlRequest<'a> {
    query: &'a str,
    variables: Variables<'a>,
}

#[derive(Serialize)]
struct Variables<'a> {
    owner: &'a str,
    name: &'a str,
    after: Option<&'a str>,
    first: u32,
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
    #[serde(default)]
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct GraphQlData {
    repository: Option<RepoData>,
}

#[derive(Deserialize)]
struct RepoData {
    issues: IssuesConn,
}

#[derive(Deserialize)]
struct IssuesConn {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    nodes: Vec<RawIssue>,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct RawIssue {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    labels: Option<LabelConn>,
}

#[derive(Deserialize)]
struct LabelConn {
    nodes: Vec<LabelNode>,
}

#[derive(Deserialize)]
struct LabelNode {
    name: String,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::response::Json as AxumJson;
    use axum::routing::post;
    use serde_json::{Value, json};

    use super::*;

    /// Spawn an axum server that returns responses from a queue. Each POST
    /// pops the next response; if the queue is empty, returns an empty data
    /// block. Returns (base_url, handle).
    async fn spawn_fake(
        responses: Vec<Value>,
    ) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        let queue = Arc::new(Mutex::new(responses));
        let state = queue.clone();
        let app = Router::new()
            .route(
                "/graphql",
                post(move |State(q): State<Arc<Mutex<Vec<Value>>>>, _body: String| async move {
                    let resp = {
                        let mut g = q.lock().unwrap();
                        if g.is_empty() {
                            json!({"data": {"repository": {"issues": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []}}}})
                        } else {
                            g.remove(0)
                        }
                    };
                    AxumJson(resp)
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/graphql");
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, queue, handle)
    }

    fn page(nodes: Vec<Value>, has_next: bool, end_cursor: Option<&str>) -> Value {
        json!({
            "data": {
                "repository": {
                    "issues": {
                        "pageInfo": {
                            "hasNextPage": has_next,
                            "endCursor": end_cursor,
                        },
                        "nodes": nodes,
                    }
                }
            }
        })
    }

    fn issue(number: u64, title: &str, labels: &[&str]) -> Value {
        json!({
            "number": number,
            "title": title,
            "body": format!("body of {number}"),
            "state": "OPEN",
            "updatedAt": "2026-04-17T00:00:00Z",
            "labels": {
                "nodes": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            }
        })
    }

    #[tokio::test]
    async fn list_open_issues_single_page() {
        let responses = vec![page(
            vec![issue(1, "first", &["bug"]), issue(2, "second", &[])],
            false,
            None,
        )];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[0].title, "first");
        assert_eq!(issues[0].labels, vec!["bug".to_string()]);
        assert_eq!(issues[0].state, GhState::Open);
        assert_eq!(issues[1].labels, Vec::<String>::new());
    }

    #[tokio::test]
    async fn list_open_issues_paginates() {
        let responses = vec![
            page(vec![issue(1, "a", &[])], true, Some("cursor1")),
            page(vec![issue(2, "b", &[])], false, None),
        ];
        let (url, queue, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[1].number, 2);
        // Both canned pages consumed
        assert!(queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn graphql_errors_propagate() {
        let responses = vec![json!({
            "errors": [{"message": "Bad credentials"}]
        })];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let err = client.list_open_issues("own", "repo").await.unwrap_err();
        match err {
            GhError::GraphQl(msg) => assert!(msg.contains("Bad credentials")),
            other => panic!("expected GraphQl error, got {other:?}"),
        }
    }

    /// GitHub answers an unresolvable issue alias with a null node plus an
    /// errors entry; the resolvable siblings in the same batch must survive.
    #[tokio::test]
    async fn issue_close_info_parses_aliases_and_tolerates_dead_numbers() {
        let responses = vec![json!({
            "data": {
                "repository": {
                    "i1": {"number": 1, "state": "CLOSED", "stateReason": "COMPLETED"},
                    "i2": {"number": 2, "state": "CLOSED", "stateReason": "NOT_PLANNED"},
                    "i3": {"number": 3, "state": "OPEN", "stateReason": null},
                    "i4": null,
                }
            },
            "errors": [{"message": "Could not resolve to an Issue with the number of 4."}]
        })];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let info = client
            .issue_close_info("own", "repo", &[1, 2, 3, 4])
            .await
            .unwrap();

        assert_eq!(
            info.get(&1),
            Some(&IssueCloseInfo {
                state: GhState::Closed,
                state_reason: Some("COMPLETED".into()),
            })
        );
        assert_eq!(
            info.get(&2).and_then(|i| i.state_reason.as_deref()),
            Some("NOT_PLANNED")
        );
        assert_eq!(info.get(&3).map(|i| i.state), Some(GhState::Open));
        assert!(!info.contains_key(&4), "dead number stays absent");
    }

    #[tokio::test]
    async fn issue_close_info_fails_when_data_is_absent() {
        let responses = vec![json!({
            "errors": [{"message": "Bad credentials"}]
        })];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let err = client
            .issue_close_info("own", "repo", &[1])
            .await
            .unwrap_err();
        match err {
            GhError::GraphQl(msg) => assert!(msg.contains("Bad credentials")),
            other => panic!("expected GraphQl error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_repository_returns_empty_list() {
        let (url, _q, _h) = spawn_fake(vec![]).await;
        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();
        assert!(issues.is_empty());
    }
}
