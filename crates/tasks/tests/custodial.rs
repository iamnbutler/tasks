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
    Actor, Capability, CharterLevel, DecisionAction, DecisionState, GhState, Project, ProjectId,
    ProjectStatus, Task, TaskId, TaskState,
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

impl Seen {
    /// Nothing at all reached GitHub. Every vector, rather than the one the
    /// route under test would have written to: the question a refusal has to
    /// answer is "did anything happen", and naming one field per route is how
    /// the tenth route gets tested against the ninth route's evidence.
    fn is_untouched(&self) -> bool {
        self.created.is_empty()
            && self.closed.is_empty()
            && self.comments.is_empty()
            && self.review_comments.is_empty()
            && self.labels_set.is_empty()
            && self.merged.is_empty()
            && self.prs_patched.is_empty()
    }
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
                        "state": "open",
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
            format!("orchestrator {}", self.store.actor_token().expose()),
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
    let (rest_url, seen) = spawn_fake_github(900).await;
    build_harness(with_github.then_some(rest_url), seen, None).await
}

/// The same harness pointed at a GitHub of the caller's choosing — for the
/// tests that need one which never answers, or which refuses.
async fn harness_against(rest_url: String) -> Harness {
    build_harness(Some(rest_url), Arc::new(Mutex::new(Seen::default())), None).await
}

/// `store` is one the caller already holds — for a fake GitHub that has to
/// read the ledger at the instant a write arrives, which needs the store to
/// exist before the fake does.
async fn build_harness(
    rest_url: Option<String>,
    seen: Arc<Mutex<Seen>>,
    store: Option<Arc<Store>>,
) -> Harness {
    let store = match store {
        Some(store) => store,
        None => Arc::new(Store::open_in_memory().await.unwrap()),
    };
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
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();

    let github = rest_url.map(|rest_url| {
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
        scout_directions: None,
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
    assert_eq!(
        decisions.len(),
        1,
        "one row, not an intent plus a confirmation"
    );
    assert_eq!(decisions[0].actor, Actor::Orchestrator);
    assert_eq!(decisions[0].action.as_str(), "capture_work");
    assert!(decisions[0].rationale.as_ref().unwrap().contains("lost"));
    // The subject is the **title**, not the task: the intent is recorded
    // before the call that would create an issue, so no issue number and no
    // task exists to point at yet. That is what the shadow branch has always
    // done ("a shadow row is a record of judgment, not a foreign key"), and
    // both halves of the capability now read alike.
    assert_eq!(
        decisions[0].subject_id,
        "store.rs leaks a transaction on the error path",
    );
    assert_eq!(decisions[0].subject_kind, "capture");
    assert_eq!(decisions[0].state, DecisionState::Applied);
    assert_eq!(
        decisions[0].outcome.as_ref().unwrap()["result"],
        900,
        "and what it produced is on the row it settled"
    );

    // The task linkage is the event's `decision_seq`, which is preserved.
    let captured = h
        .store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.payload {
            tasks::events::EventPayload::IssueCaptured {
                task_id,
                decision_seq,
                ..
            } => Some((task_id, decision_seq)),
            _ => None,
        })
        .expect("the capture is on the event feed");
    assert_eq!(captured.0, task.id);
    assert_eq!(captured.1, Some(decisions[0].seq));
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
    // And the issue is still open upstream. The sentence above claimed this
    // from the day the test was written; it was true of the intent and false
    // of the code, because `require_rationale` sat inside `record_issue_closed`
    // and that runs *after* `github.close_issue` (#957).
    assert!(h.seen.lock().unwrap().closed.is_empty());
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

/// The reported shape of #957: a rationale-less capture 400s, the agent does
/// the obviously correct thing with a 4xx and retries, and each retry files
/// another issue nothing in the ledger accounts for.
///
/// A 400 has to be a no-op, or the one thing the status code tells a caller to
/// do is the thing it must not do.
#[tokio::test]
async fn a_rejected_capture_files_nothing_however_many_times_it_is_retried() {
    let h = harness(true).await;

    for _ in 0..3 {
        let resp = h
            .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
            .json(&json!({
                "title": "store.rs leaks a transaction on the error path",
                "body": "Noticed while reading the review code.",
                "provenance": "discovered while reviewing spec_abc",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["error"].as_str().unwrap(),
            "an orchestrator decision must carry a rationale",
        );
    }

    // Three refusals, three no-ops: nothing upstream, nothing tracked, and
    // nothing in the ledger to explain any of it.
    assert!(h.seen.lock().unwrap().created.is_empty());
    assert!(h.store.decisions(None, 10).await.unwrap().is_empty());
    assert!(h.store.list_tasks().await.unwrap().is_empty());
}

/// The durable one. Every charter-gated write route, called with no rationale,
/// answers 400 having touched nothing — and it is asserted against the whole
/// of what the fake GitHub saw rather than against the one call each route
/// makes, so a tenth route that forgets fails here rather than on a repository.
///
/// This holds because the check lives at `authorize`, which every one of these
/// handlers already called before its effect. A per-handler `if
/// rationale.is_empty()` is what three of these nine already had, and the six
/// that did not are the bug.
#[tokio::test]
async fn no_write_route_touches_github_before_it_has_a_rationale() {
    let h = harness(true).await;
    let task = seed_task(&h.store, &h.project, 812).await;

    let routes: Vec<(String, Value)> = vec![
        (
            "/issues".into(),
            // Provenance supplied, so the refusal under test is the rationale
            // and not the capture route's own earlier check.
            json!({ "title": "x", "body": "", "provenance": "reviewing #812" }),
        ),
        (
            format!("/tasks/{}/close", task.id),
            json!({ "reason": "completed" }),
        ),
        (format!("/tasks/{}/reopen", task.id), json!({})),
        ("/issues/812/comments".into(), json!({ "body": "a note" })),
        (
            "/issues/812/edit".into(),
            json!({ "title": "a better title" }),
        ),
        ("/issues/812/labels".into(), json!({ "labels": ["bug"] })),
        (
            "/pull-requests/812/review-comments".into(),
            json!({ "path": "crates/tasks/src/store.rs", "line": 12, "body": "this leaks" }),
        ),
        ("/pull-requests/812/merge".into(), json!({})),
        ("/pull-requests/812/close".into(), json!({})),
    ];

    for (path, body) in &routes {
        let resp = h
            .as_orchestrator(h.http.post(format!("{}{path}", h.base)))
            .json(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{path} should refuse");
        assert!(
            h.seen.lock().unwrap().is_untouched(),
            "{path} reached GitHub before it had a rationale",
        );
    }

    // Nine refusals, and not one of them recorded a judgment either — there
    // was no judgment to record.
    assert!(h.store.decisions(None, 20).await.unwrap().is_empty());
}

/// Shadow was never the leaky half — it records first and reaches GitHub never
/// — but it is where a rationale matters most, because the row *is* the
/// deliverable. A shadow row with an empty rationale is the unreviewable
/// artifact the rule exists to prevent, so the check runs ahead of the shadow
/// branch rather than inside it.
#[tokio::test]
async fn a_shadowed_write_still_needs_a_rationale() {
    let h = harness(true).await;
    h.store
        .set_charter(Capability::CaptureWork, CharterLevel::Shadow, None)
        .await
        .unwrap();

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({ "title": "x", "body": "", "provenance": "reviewing #812" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(h.store.decisions(None, 10).await.unwrap().is_empty());
    assert!(h.seen.lock().unwrap().is_untouched());

    // With one, the shadow row lands as it always did.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({
            "title": "x",
            "body": "",
            "provenance": "reviewing #812",
            "rationale": "it would be lost otherwise",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].enforced);
    assert!(h.seen.lock().unwrap().is_untouched());
}

/// `Off` answers before the rationale does, and the status code is how you can
/// tell: a rationale cannot rescue a capability that was never going to act,
/// so sending the caller away to write one would send it to fix the wrong
/// thing about a call that will 403 either way.
#[tokio::test]
async fn a_capability_that_is_off_is_refused_before_the_rationale_is_read() {
    let h = harness(true).await;
    h.store
        .set_charter(Capability::CaptureWork, CharterLevel::Off, None)
        .await
        .unwrap();

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({ "title": "x", "body": "", "provenance": "reviewing #812" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("off in the charter"),
        "{body}",
    );
    assert!(h.seen.lock().unwrap().is_untouched());
}

/// The human is never gated and owes no explanation — they are who the record
/// is *for* — so the reordering must not have quietly made them owe one. The
/// issue is filed.
#[tokio::test]
async fn a_human_still_needs_no_rationale() {
    let h = harness(true).await;

    let resp = h
        .http
        .post(format!("{}/issues", h.base))
        .json(&json!({ "title": "something", "body": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(h.seen.lock().unwrap().created.len(), 1);
}

/// A GitHub that never answers — every write is a 503, which
/// `GhError::is_unavailable` reads as "no answer" rather than as a refusal —
/// **and that reads the ledger at the moment each write arrives**.
///
/// That second half is what makes the guard about *ordering* rather than about
/// the end state. A handler that recorded its intent after the call would
/// still leave a pending row behind, and an end-state assertion would pass;
/// what it must not be able to do is reach GitHub with nothing on record, and
/// the only place that is observable is inside the request. The fake runs in
/// this process, so it can simply ask the store.
///
/// `GET /issues/{n}` still answers, because `edit_issue` reads the text it is
/// about to replace *before* it records its intent — a read is not an effect,
/// and the old text belongs in the immutable `evidence` half of the row.
///
/// The explicit `patch` handlers matter: a `MethodRouter` that matches a path
/// answers **405** itself rather than falling through to the router's
/// `fallback`, and 405 is GitHub *answering*, which is the other branch
/// entirely.
async fn spawn_silent_github(store: Arc<Store>) -> String {
    async fn unavailable(
        State(store): State<Arc<Store>>,
    ) -> (axum::http::StatusCode, AxumJson<Value>) {
        // In the `message`, deliberately: that is the field `rest_error`
        // carries into `GhError`, so it survives into the `outcome` the test
        // reads back. Anything else here would be dropped.
        let pending = store.pending_decisions().await.unwrap().len();
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            AxumJson(json!({ "message": format!("upstream is down; pending_at_call={pending}") })),
        )
    }
    let app = axum::Router::new()
        .route(
            "/repos/{owner}/{repo}/issues/{number}",
            axum::routing::get(
                move |AxumPath((_o, _r, number)): AxumPath<(String, String, u64)>| async move {
                    AxumJson(json!({
                        "number": number,
                        "title": "the old title",
                        "body": "the old body",
                    }))
                },
            )
            .patch(unavailable),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            axum::routing::get(unavailable).patch(unavailable),
        )
        .fallback(unavailable)
        .with_state(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// The route that produces one `DecisionAction`, or `None` when the action
/// never reaches another system.
///
/// **This match is the guard.** `DecisionAction::ALL` is complete by
/// construction (the enum is macro-generated), so a new variant does not
/// compile here until somebody says whether it reaches GitHub — and the test
/// below then drives it without anyone remembering to add it to a list. A
/// hand-written list of nine routes is green on the day the tenth is added,
/// which is exactly the day it needed to be red.
///
/// The one residue: a *new route reusing an existing action* is not covered,
/// because this drives one request per action. That is narrower than what was
/// claimed before, and it is stated rather than assumed.
fn write_route(action: DecisionAction, task: &Task) -> Option<(String, Value)> {
    match action {
        DecisionAction::CaptureWork => Some((
            "/issues".into(),
            json!({
                "title": "an issue nobody knows the fate of",
                "body": "",
                "provenance": "reviewing #812",
                "rationale": "it would be lost otherwise",
            }),
        )),
        DecisionAction::RetireWork => Some((
            format!("/tasks/{}/close", task.id),
            json!({ "reason": "completed", "rationale": "the PR that implements it landed" }),
        )),
        DecisionAction::ReopenWork => Some((
            format!("/tasks/{}/reopen", task.id),
            json!({ "rationale": "closing it was wrong" }),
        )),
        DecisionAction::CommentOnWork => Some((
            "/issues/812/comments".into(),
            json!({ "body": "a note", "rationale": "the reviewer asked" }),
        )),
        DecisionAction::ReviewComment => Some((
            "/pull-requests/812/review-comments".into(),
            json!({
                "path": "crates/tasks/src/store.rs",
                "line": 12,
                "body": "this leaks",
                "rationale": "it points at code",
            }),
        )),
        DecisionAction::EditIssue => Some((
            "/issues/812/edit".into(),
            json!({ "title": "a better title", "rationale": "the theory collapsed" }),
        )),
        DecisionAction::LabelIssue => Some((
            "/issues/812/labels".into(),
            json!({ "labels": ["bug"], "rationale": "it is one" }),
        )),
        DecisionAction::MergeBuild => Some((
            "/pull-requests/812/merge".into(),
            json!({ "rationale": "the build reported a passing run and the base is the trunk" }),
        )),
        DecisionAction::AbandonBuild => Some((
            "/pull-requests/812/close".into(),
            json!({ "rationale": "the branch will not land" }),
        )),
        // No effect in anybody else's system: these commit in the same
        // transaction as the state they authorize, so there is no window to
        // represent and they are never written pending.
        DecisionAction::Approve
        | DecisionAction::NeedsRevision
        | DecisionAction::Reject
        | DecisionAction::RequestBuild
        | DecisionAction::AuthorSpec
        | DecisionAction::QueueTask
        | DecisionAction::CancelRun
        | DecisionAction::SettleDecision => None,
    }
}

/// **The durable one.** Every route that reaches GitHub, driven against a
/// GitHub that never answers, must leave exactly one `pending` row behind —
/// which is to say it recorded its intent *before* the call.
///
/// Driven off `DecisionAction::ALL` rather than a hand-written list, so a
/// tenth route fails here rather than on a repository. See [`write_route`].
#[tokio::test]
async fn no_write_route_reaches_github_without_recording_first() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let rest_url = spawn_silent_github(store.clone()).await;
    let h = build_harness(
        Some(rest_url),
        Arc::new(Mutex::new(Seen::default())),
        Some(store),
    )
    .await;
    let task = seed_task(&h.store, &h.project, 812).await;

    let mut driven = 0;
    for action in DecisionAction::ALL {
        let Some((path, body)) = write_route(*action, &task) else {
            continue;
        };
        driven += 1;
        let resp = h
            .as_orchestrator(h.http.post(format!("{}{path}", h.base)))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            503,
            "{path}: GitHub never answered, so neither may we claim it did"
        );

        let pending = h.store.pending_decisions().await.unwrap();
        let mine: Vec<_> = pending.iter().filter(|d| d.action == *action).collect();
        assert_eq!(
            mine.len(),
            1,
            "{path} left {} pending rows for {}",
            mine.len(),
            action.as_str()
        );
        assert!(
            mine[0].outcome.as_ref().unwrap().get("intent").is_some(),
            "{path}: a pending row with no intent cannot be reconciled"
        );
        let unanswered = mine[0].outcome.as_ref().unwrap()["unanswered"]
            .as_str()
            .expect("and it should say what came back")
            .to_string();
        // **The ordering assertion**, and the one an end-state check cannot
        // make: the fake read the ledger at the instant the call landed, and
        // this route's intent was already in it. A handler that recorded
        // afterwards would still leave a pending row behind — it would just
        // have reached GitHub with nothing on record first, which is the whole
        // failure.
        assert!(
            unanswered.contains(&format!("pending_at_call={driven}")),
            "{path} reached GitHub before its intent was on record: {unanswered}"
        );
    }

    assert_eq!(driven, 9, "nine routes reach GitHub today");
    assert_eq!(
        h.store.pending_decisions().await.unwrap().len(),
        driven,
        "one row each, and nothing settled itself"
    );
}

/// GitHub *answered* — 4xx — so nothing reached the world and the row is
/// annulled rather than left open. The two are not the same fact and the
/// difference is the whole design: `pending` means nobody knows.
#[tokio::test]
async fn a_refused_write_is_annulled_rather_than_left_pending() {
    let app = axum::Router::new().fallback(|| async {
        (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            AxumJson(json!({ "message": "Validation Failed" })),
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let h = harness_against(url).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({
            "title": "x",
            "body": "",
            "provenance": "reviewing #812",
            "rationale": "it would be lost otherwise",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);

    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].state, DecisionState::Annulled);
    assert!(decisions[0].settled_at.is_some());
    assert!(
        h.store.pending_decisions().await.unwrap().is_empty(),
        "GitHub said no; there is no window"
    );
    assert!(
        decisions[0].outcome.as_ref().unwrap()["refused"]
            .as_str()
            .unwrap()
            .contains("Validation Failed"),
        "and GitHub's own message is what makes it readable: {:?}",
        decisions[0].outcome
    );
}

/// Discharging a pending row does not require the caller to hold a GitHub
/// credential: the **server** looks the artifact up and says what it found,
/// and the settle is written from that.
///
/// This is what makes `reconcile_decision` an obligation its recipient can
/// actually discharge — the orchestrator runs `--allowedTools Bash(curl:*)`
/// with no `GITHUB_TOKEN`, so an obligation needing its own GitHub read would
/// leave it a choice between guessing and doing nothing.
#[tokio::test]
async fn a_pending_decision_is_reconciled_from_evidence_the_server_produced() {
    let h = harness(true).await;
    let task = seed_task(&h.store, &h.project, 812).await;

    // A close whose ledger row never learned what happened.
    let seq = h
        .store
        .record_intent(
            "task",
            task.id.as_str(),
            DecisionAction::RetireWork,
            &tasks::models::DecisionInput {
                actor: Actor::Orchestrator,
                rationale: Some("the PR that implements it landed".into()),
                evidence: None,
            },
            Some(&json!({
                "repo": format!("{}/{}", h.project.repo_owner, h.project.repo_name),
                "issue": 812,
                "reason": "completed",
            })),
        )
        .await
        .unwrap();

    // It is listed as pending over HTTP, which is how the recipient finds it.
    let listed: Vec<Value> = h
        .http
        .get(format!("{}/decisions?pending=true", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["seq"], seq);
    assert_eq!(listed[0]["state"], "pending");

    // Settling it back to `pending` is a 400: that is where it already is, and
    // a settle that leaves the window open is a no-op with a ledger row.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/decisions/{seq}/settle", h.base)))
        .json(&json!({ "state": "pending", "rationale": "still unsure" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // The fake answers `GET /issues/812` as open, so the honest verdict is
    // that the close never landed.
    let found: Value = h
        .as_orchestrator(h.http.get(format!("{}/decisions/{seq}/reconcile", h.base)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["verdict"], "annulled", "{found}");
    assert_eq!(found["found"]["issue"], 812);

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/decisions/{seq}/settle", h.base)))
        .json(&json!({
            "state": "annulled",
            "rationale": "the server read #812 back and it is open",
            "outcome": { "reconciled_from": "GET /decisions/{seq}/reconcile" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let settled = h.store.decision(seq).await.unwrap().unwrap();
    assert_eq!(settled.state, DecisionState::Annulled);
    assert!(
        settled.outcome.as_ref().unwrap().get("intent").is_some(),
        "the intent survives the settle: json_patch merges, it does not replace"
    );
    assert_eq!(
        settled.outcome.as_ref().unwrap()["reconciled_from"],
        "GET /decisions/{seq}/reconcile"
    );

    // And the reconciliation said who made it, in the same transaction.
    let own = h
        .store
        .decisions(Some(("decision", &seq.to_string())), 5)
        .await
        .unwrap();
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].action, DecisionAction::SettleDecision);
    assert_eq!(own[0].actor, Actor::Orchestrator);

    // A second settle is a refusal, not a silent no-op.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/decisions/{seq}/settle", h.base)))
        .json(&json!({ "state": "applied", "rationale": "changed my mind" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// A capability demoted to `off` while a row is pending must not make that row
/// undischargeable — which is the exact property the obligation claims.
///
/// Settling is not the action: the effect already happened, and refusing to
/// record it does not un-file the issue, it only keeps the ledger wrong. So a
/// settle is never charter-gated, and the response says which capability the
/// settled row came from so a reader can see it has since been switched off.
#[tokio::test]
async fn a_settle_is_not_refused_by_a_capability_since_demoted() {
    let h = harness(true).await;
    let seq = h
        .store
        .record_intent(
            "capture",
            "an issue nobody knows the fate of",
            DecisionAction::CaptureWork,
            &tasks::models::DecisionInput {
                actor: Actor::Orchestrator,
                rationale: Some("it would be lost otherwise".into()),
                evidence: None,
            },
            None,
        )
        .await
        .unwrap();

    // Demotion is most likely exactly when something has gone wrong, which is
    // when pending rows exist.
    h.store
        .set_charter(Capability::CaptureWork, CharterLevel::Off, None)
        .await
        .unwrap();

    // The capture route itself is now refused, as it must be.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/issues", h.base)))
        .json(&json!({
            "title": "another",
            "body": "",
            "provenance": "reviewing #812",
            "rationale": "because",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // The settle is not.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/decisions/{seq}/settle", h.base)))
        .json(&json!({
            "state": "applied",
            "rationale": "the server found the issue upstream",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "a nag its recipient is forbidden to discharge is what must not ship"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["capability"], "capture_work");
    assert_eq!(
        h.store.decision(seq).await.unwrap().unwrap().state,
        DecisionState::Applied
    );
}
