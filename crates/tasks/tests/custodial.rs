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
use axum::routing::{post, put};
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
    /// Every PATCH to `/issues/{n}` — closes and reopens both land here,
    /// because they are the same GitHub call with a different `state`.
    closed: Vec<(u64, Value)>,
    comments: Vec<(u64, Value)>,
    review_comments: Vec<(u64, Value)>,
    labels_set: Vec<(u64, Value)>,
    merged: Vec<(u64, Value)>,
    prs_patched: Vec<(u64, Value)>,
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
            axum::routing::get(
                move |AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>| async move {
                    AxumJson(json!({
                        "number": number,
                        "title": "the old title",
                        "body": "the old body, resting on a theory that collapsed",
                    }))
                },
            )
            .patch(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().closed.push((number, body));
                    AxumJson(json!({ "number": number, "state": "closed" }))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/labels",
            axum::routing::get(|| async {
                AxumJson(json!([
                    { "name": "bug", "description": "something is broken" },
                    { "name": "enhancement", "description": null },
                ]))
            }),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            post(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().comments.push((number, body));
                    AxumJson(json!({ "id": 4242 }))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/comments",
            post(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().review_comments.push((number, body));
                    AxumJson(json!({ "id": 77 }))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels",
            put(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().labels_set.push((number, body));
                    AxumJson(json!([]))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/merge",
            put(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().merged.push((number, body));
                    AxumJson(json!({ "sha": "deadbeef", "merged": true }))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            axum::routing::get(
                move |AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>| async move {
                    AxumJson(json!({ "number": number, "head": { "sha": "abc123" } }))
                },
            )
            .patch(
                move |State(s): State<Arc<Mutex<Seen>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    s.lock().unwrap().prs_patched.push((number, body));
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
    for capability in [
        Capability::CaptureWork,
        Capability::RetireWork,
        Capability::CommentOnWork,
        Capability::LandBuilds,
        Capability::CurateWork,
    ] {
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

    let app = tasks::server::router_with_services(
        store.clone(),
        tasks::server::Services {
            github,
            ..Default::default()
        },
    );
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

/// A verdict has somewhere to go. Before this route existed, the orchestrator
/// could review a PR and then only *describe* the review, which meant a human
/// re-read the reasoning and re-typed it — work done twice and kept once.
#[tokio::test]
async fn a_comment_reaches_github_signed_and_recorded() {
    let h = harness(true).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues/837/comments", h.base)))
        .json(&json!({
            "body": "The migration in this branch claims 0016, which main already uses.",
            "rationale": "blocking defect found while reviewing the diff",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Posted, against the GitHub number rather than any local id — a Builder's
    // PR has no task of its own to address.
    let comments = h.seen.lock().unwrap().comments.clone();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].0, 837);
    let body = comments[0].1["body"].as_str().unwrap();
    assert!(body.contains("claims 0016"), "{body}");
    // Signed, because a reader on GitHub has no access to the ledger and
    // should not have to guess whether a person wrote this.
    assert!(body.contains("Posted by the Tasks orchestrator"), "{body}");

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].action.as_str(), "comment_on_work");
    assert_eq!(decisions[0].actor, Actor::Orchestrator);
    assert_eq!(decisions[0].subject_id, "837");
}

/// A human's comment goes up as written. The footer is attribution, not
/// branding — it exists to distinguish the machine, so putting it on a
/// person's words would be a small lie.
#[tokio::test]
async fn a_human_comment_is_not_signed() {
    let h = harness(true).await;

    h.http
        .post(format!("{}/issues/837/comments", h.base))
        .json(&json!({ "body": "looks good" }))
        .send()
        .await
        .unwrap();

    let comments = h.seen.lock().unwrap().comments.clone();
    assert_eq!(comments[0].1["body"].as_str().unwrap(), "looks good");

    // Still a ledger row, attributed to the human — the ledger records what
    // the system did, not only what it did unsupervised, and a comment with
    // no row would leave a gap right where a reader is comparing the two.
    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].actor, Actor::Human);
}

/// Merging is the one write whose recourse is a revert rather than an edit, so
/// the ledger row has to be worth reading on its own. An autonomous merge with
/// no stated reason is refused before anything reaches GitHub.
#[tokio::test]
async fn an_autonomous_merge_states_why_or_does_not_happen() {
    let h = harness(true).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/pull-requests/837/merge", h.base)))
        .json(&json!({ "method": "squash" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(
        h.seen.lock().unwrap().merged.is_empty(),
        "refused before the write"
    );

    // With a reason, it lands — and the SHA comes back from GitHub rather than
    // from anything we predicted.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/pull-requests/837/merge", h.base)))
        .json(&json!({
            "method": "squash",
            "rationale": "CI green, diff matches the approved spec, no migration collision",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["merged_sha"], "deadbeef");

    let merged = h.seen.lock().unwrap().merged.clone();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].0, 837);
    assert_eq!(merged[0].1["merge_method"], "squash");

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "merge_build");
    assert!(
        decisions[0]
            .rationale
            .as_ref()
            .unwrap()
            .contains("CI green")
    );
}

/// Closing a PR unmerged discards work that cost a VM hour, so it answers to
/// the same standard as merging it.
#[tokio::test]
async fn abandoning_a_build_states_why_and_is_recorded() {
    let h = harness(true).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/pull-requests/837/close", h.base)))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(h.seen.lock().unwrap().prs_patched.is_empty());

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/pull-requests/837/close", h.base)))
        .json(&json!({ "rationale": "superseded by #834, which fixes the same leak" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let patched = h.seen.lock().unwrap().prs_patched.clone();
    assert_eq!(patched.len(), 1);
    assert_eq!(patched[0].1["state"], "closed");

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "abandon_build");
}

/// Reopening is the recourse that makes closing safe to hand over. It answers
/// to `retire_work`, not a capability of its own: the power to retire and the
/// power to take it back are the same power, and a charter that could switch
/// off only the undo would be a strange kind of safety.
#[tokio::test]
async fn a_retirement_can_be_taken_back() {
    let h = harness(true).await;
    let task = seed_task(&h.store, &h.project, 812).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/tasks/{}/reopen", h.base, task.id)))
        .json(&json!({ "rationale": "closed as completed, but the Makefile call is still there" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let patched = h.seen.lock().unwrap().closed.clone();
    assert_eq!(patched.len(), 1);
    assert_eq!(patched[0].0, 812);
    assert_eq!(patched[0].1["state"], "open");

    // Nothing is marked open locally: open-or-closed is GitHub's fact, and the
    // poller reads it back like any issue reopened in a browser.
    let after = h.store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(after.gh_state, GhState::Open);

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "reopen_work");
    assert!(
        decisions[0]
            .rationale
            .as_ref()
            .unwrap()
            .contains("still there")
    );
}

/// A review comment lands on the line it is about, anchored to the head SHA
/// the server read rather than one the caller supplied.
#[tokio::test]
async fn a_review_comment_is_anchored_to_a_line_and_a_live_sha() {
    let h = harness(true).await;

    let resp = h
        .as_orchestrator(
            h.http
                .post(format!("{}/pull-requests/837/review-comments", h.base)),
        )
        .json(&json!({
            "path": "Makefile",
            "line": 12,
            "body": "The CARGO=/nonexistent-cargo test was dropped here.",
            "rationale": "it is the only thing making the claim falsifiable",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    let seen = h.seen.lock().unwrap().review_comments.clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, 837);
    assert_eq!(seen[0].1["path"], "Makefile");
    assert_eq!(seen[0].1["line"], 12);
    // The SHA came from GitHub at comment time. A SHA that arrived through a
    // prompt is stale by construction — the branch may have moved since.
    assert_eq!(seen[0].1["commit_id"], "abc123");

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "review_comment");
    assert_eq!(decisions[0].subject_id, "837#Makefile:12");
}

/// Editing is the one write that destroys. The ledger keeps what was
/// overwritten without being asked, because "the orchestrator edited #835" is
/// not a record anyone can recover from — the diff is.
#[tokio::test]
async fn an_edit_keeps_the_text_it_replaced() {
    let h = harness(true).await;

    // No reason, no edit: once the old text lives only in the ledger, an
    // unexplained rewrite is indistinguishable from a mistake.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues/835/edit", h.base)))
        .json(&json!({ "body": "new body" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(h.seen.lock().unwrap().closed.is_empty());

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues/835/edit", h.base)))
        .json(&json!({
            "body": "Run 6 survived 937s, so the ~370s boundary was two coincidences.",
            "rationale": "the failure boundary I wrote this around did not hold",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let patched = h.seen.lock().unwrap().closed.clone();
    assert_eq!(patched.len(), 1);
    assert!(
        patched[0].1["body"]
            .as_str()
            .unwrap()
            .contains("coincidences"),
        "{:?}",
        patched[0].1
    );

    // The replaced text is on the decision, unasked.
    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "edit_issue");
    let evidence = decisions[0].evidence.as_ref().expect("the old text");
    assert!(
        evidence["replaced"]["body"]
            .as_str()
            .unwrap()
            .contains("theory that collapsed"),
        "{evidence}"
    );
    assert_eq!(evidence["replaced"]["title"], "the old title");
}

/// Labels are settable, and the vocabulary is readable — the second is what
/// makes the first honest. With no way to ask what labels exist, the only
/// truthful thing a caller can do is file with none.
#[tokio::test]
async fn labels_can_be_read_and_replaced() {
    let h = harness(true).await;

    let labels: Vec<Value> = h
        .http
        .get(format!("{}/labels", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0]["name"], "bug");
    assert_eq!(labels[0]["description"], "something is broken");

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues/835/labels", h.base)))
        .json(&json!({ "labels": ["bug"], "rationale": "it describes a defect, not a request" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // The complete set, so removing a label is expressible.
    let set = h.seen.lock().unwrap().labels_set.clone();
    assert_eq!(set[0].1["labels"], json!(["bug"]));

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions[0].action.as_str(), "label_issue");
}
