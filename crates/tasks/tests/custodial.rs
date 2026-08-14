//! The custodial writes — filing and retiring work — end to end over HTTP.
//!
//! GitHub is a real axum server on loopback speaking the two REST routes we
//! call, the same shape `tests/builder.rs` uses for PR creation. That keeps the
//! assertion honest in the direction that matters: what we actually *send* to
//! GitHub, and what we record locally afterwards.
//!
//! The point of these routes is not that the orchestrator gains a capability —
//! it can already file issues with its own `gh` credential. The point is that
//! the capability stops happening outside the system, so every one of these
//! tests is really asking "is there a record?".

use std::sync::{Arc, Mutex};

use axum::Json as AxumJson;
use axum::extract::{Path as AxumPath, State};
use axum::routing::{patch, post};
use chrono::Utc;
use serde_json::{Value, json};
use tasks::models::{
    Actor, Capability, CharterLevel, GhState, Project, ProjectId, Task, TaskId, TaskState,
};
use tasks::store::Store;

/// What the fake GitHub saw, so a test can assert on the request rather than
/// only on our own bookkeeping.
#[derive(Default)]
struct Seen {
    created: Vec<Value>,
    closed: Vec<(u64, Value)>,
}

async fn spawn_fake_github(issue_number: u64) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = axum::Router::new()
        .route(
            "/repos/{owner}/{repo}/issues",
            post(
                move |State(s): State<Arc<Mutex<Seen>>>, AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().created.push(body);
                    AxumJson(json!({ "number": issue_number }))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}",
            patch(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().closed.push((number, body));
                    AxumJson(json!({ "number": number, "state": "closed" }))
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

struct Harness {
    store: Arc<Store>,
    base: String,
    project: Project,
    seen: Arc<Mutex<Seen>>,
    http: reqwest::Client,
}

impl Harness {
    /// A request as the orchestrator: carrying the token the server minted for
    /// it, which is the whole basis of attribution.
    fn as_orchestrator(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.header(
            "X-Tasks-Actor",
            format!("orchestrator {}", self.store.actor_token()),
        )
    }
}

/// A tasks server with a GitHub client pointed at the fake, plus one project.
/// `github` is `None` to exercise the unconfigured path.
///
/// The custodial capabilities are turned on here, because these tests are
/// about the write path rather than about the charter — `tests/charter.rs`
/// owns the question of whether they are permitted at all, and the default for
/// both is `off`.
async fn harness(with_github: bool) -> Harness {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    for capability in [Capability::CaptureWork, Capability::RetireWork] {
        store
            .set_charter(capability, CharterLevel::Live, None)
            .await
            .unwrap();
    }
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();

    let (rest_url, seen) = spawn_fake_github(900).await;
    let github = with_github.then(|| {
        Arc::new(
            tasks::github::GitHubClient::with_base_url("token", "http://unused.invalid/graphql")
                .with_rest_base_url(rest_url),
        )
    });

    let app = tasks::server::router_with_services(store.clone(), None, github);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Harness {
        store,
        base,
        project,
        seen,
        http: reqwest::Client::new(),
    }
}

async fn seed_task(store: &Store, project: &Project, issue: u64) -> Task {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: issue,
        title: "an old idea".into(),
        body: "body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::Backlog,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    task
}

/// The capture path: the issue is filed upstream, tracked here, attributed,
/// and explained — and it lands in the backlog rather than the queue.
#[tokio::test]
async fn a_captured_issue_is_filed_tracked_and_explained() {
    let h = harness(true).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({
            "title": "store.rs leaks a transaction on the error path",
            "body": "Noticed while reading the review code.",
            "provenance": "discovered while reviewing spec_abc for #812",
            "rationale": "out of scope for the spec that surfaced it, and it would be lost otherwise",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let task: Task = resp.json().await.unwrap();

    // Filed upstream, with the provenance footer the server adds rather than
    // trusts the caller to include.
    let created = h.seen.lock().unwrap().created.clone();
    assert_eq!(created.len(), 1);
    let body = created[0]["body"].as_str().unwrap();
    assert!(body.contains("Noticed while reading"), "{body}");
    assert!(body.contains("Filed by the Tasks orchestrator"), "{body}");
    assert!(body.contains("reviewing spec_abc for #812"), "{body}");

    // Tracked here, and only in the backlog — capture is not a decision to
    // work on something.
    assert_eq!(task.gh_issue_number, 900);
    assert_eq!(task.state, TaskState::Backlog);
    assert_eq!(task.manual_rank, None);

    // And there is a record, which is the entire reason this route exists.
    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].actor, Actor::Orchestrator);
    assert_eq!(decisions[0].action.as_str(), "capture_work");
    assert!(decisions[0].rationale.as_ref().unwrap().contains("lost"));
    assert_eq!(decisions[0].subject_id, task.id.to_string());
}

/// An autonomous capture with nowhere to trace it back to is refused. This is
/// the check that keeps the capture instinct auditable when it drifts loose —
/// the failure mode of this capability is issue spam nobody can attribute.
#[tokio::test]
async fn an_orchestrator_capture_without_provenance_is_refused() {
    let h = harness(true).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({ "title": "something", "body": "", "rationale": "because" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Nothing was filed upstream — the refusal happens before the write.
    assert!(h.seen.lock().unwrap().created.is_empty());

    // A human filing the same thing is fine: they are the accountable party
    // already, and this is attribution, not authentication.
    let resp = h
        .http
        .post(format!("{}/issues", h.base))
        .json(&json!({ "title": "something", "body": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
}

/// The retire path. The interesting assertion is the *absence*: closure is
/// GitHub's fact, so nothing local is marked closed in anticipation.
#[tokio::test]
async fn closing_a_task_writes_upstream_and_waits_for_the_poller() {
    let h = harness(true).await;
    let task = seed_task(&h.store, &h.project, 777).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/tasks/{}/close", h.base, task.id)))
        .json(&json!({
            "reason": "not_planned",
            "rationale": "superseded by the double-diamond rework; the subsystem it targets is gone",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let closed = h.seen.lock().unwrap().closed.clone();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].0, 777);
    assert_eq!(closed[0].1["state"], "closed");
    assert_eq!(closed[0].1["state_reason"], "not_planned");

    // Local state is untouched: the poller observes the closure on its next
    // pass, exactly as for an issue closed in a browser. Pre-marking it would
    // persist a GitHub-owned fact — and would make a failed close that we
    // retried look like it had worked.
    let after = h.store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(after.gh_state, GhState::Open);
    assert_eq!(after.state, TaskState::Backlog);

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "retire_work");
    assert_eq!(decisions[0].actor, Actor::Orchestrator);
}

/// An autonomous decision nobody can review afterwards is not one the
/// orchestrator was trusted to make, so the server refuses it — and refuses it
/// before touching GitHub, since the write is the irreversible half.
#[tokio::test]
async fn an_orchestrator_close_without_a_rationale_is_refused() {
    let h = harness(true).await;
    let task = seed_task(&h.store, &h.project, 777).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/tasks/{}/close", h.base, task.id)))
        .json(&json!({ "reason": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(h.store.decisions(None, 10).await.unwrap().is_empty());
}

/// Without a token the server says so rather than failing obscurely — and, in
/// particular, rather than leaving the agent to reach for its own credential.
#[tokio::test]
async fn without_a_github_token_the_write_routes_say_so() {
    let h = harness(false).await;
    let task = seed_task(&h.store, &h.project, 777).await;

    for resp in [
        h.http
            .post(format!("{}/issues", h.base))
            .json(&json!({ "title": "x", "body": "" }))
            .send()
            .await
            .unwrap(),
        h.http
            .post(format!("{}/tasks/{}/close", h.base, task.id))
            .json(&json!({ "reason": "completed" }))
            .send()
            .await
            .unwrap(),
    ] {
        assert_eq!(resp.status(), 503);
        let body: Value = resp.json().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("GITHUB_TOKEN"),
            "{body}"
        );
    }
}
