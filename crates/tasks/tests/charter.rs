//! The charter, over real HTTP.
//!
//! The question every test here asks is the same one: can the orchestrator do
//! this, and is there a record either way? Enforcement lives on the endpoint
//! rather than in the prompt because prompt text is exactly what a restarted
//! or overlong session misweighs — authority a model can talk itself into is
//! not authority.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{Value, json};
use tasks::models::{
    Actor, Capability, CharterEntry, CharterLevel, Complexity, GhState, Project, ProjectId,
    ProjectStatus, Session, SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry,
    SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::store::Store;

struct Harness {
    store: Arc<Store>,
    base: String,
    http: reqwest::Client,
    project: Project,
}

impl Harness {
    fn as_orchestrator(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.header(
            "X-Tasks-Actor",
            format!("orchestrator {}", self.store.actor_token().expose()),
        )
    }

    async fn set(&self, capability: Capability, level: CharterLevel, daily_limit: Option<i64>) {
        self.store
            .set_charter(capability, level, daily_limit)
            .await
            .unwrap();
    }
}

async fn harness() -> Harness {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();

    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Harness {
        store,
        base,
        http: reqwest::Client::new(),
        project,
    }
}

async fn seed_task(h: &Harness, issue: u64, state: TaskState) -> Task {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: h.project.id.clone(),
        gh_issue_number: issue,
        title: format!("task {issue}"),
        body: "body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
        scout_directions: None,
    };
    h.store.insert_task(&task).await.unwrap();
    task
}

async fn seed_spec(h: &Harness, issue: u64) -> Spec {
    let task = seed_task(h, issue, TaskState::InReview).await;
    let now = Utc::now();
    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: None,
        branch: "scout/x".into(),
        status: SessionStatus::ScoutSucceeded,
        started_at: now,
        completed_at: Some(now),
        exit_reason: None,
        usage: None,
        directions: None,
    };
    h.store.insert_session(&session).await.unwrap();
    let spec = Spec {
        id: SpecId::new(),
        session_id: Some(session.id),
        task_id: task.id,
        content: "## Spec".into(),
        complexity: Complexity::Simple,
        files_touched: vec![],
        created_at: now,
    };
    h.store.insert_spec(&spec).await.unwrap();
    h.store
        .upsert_spec_queue_entry(&SpecQueueEntry {
            spec_id: spec.id.clone(),
            status: SpecQueueStatus::PendingReview,
            rank: None,
            approved_at: None,
            feedback: None,
            blocking_dependencies: vec![],
        })
        .await
        .unwrap();
    spec
}

/// The shipped defaults, pinned.
///
/// Not a rubber stamp on the migration: these levels are the whole risk
/// posture of a fresh install, and the reason each is what it is does not
/// survive in anyone's head. `dispatch_builds` live alongside
/// `auto_review_specs` shadow is the specific combination worth protecting —
/// a single autonomy dial cannot express it, and it is the state this system
/// is meant to run in first.
#[tokio::test]
async fn a_fresh_install_ships_the_intended_posture() {
    let h = harness().await;
    let charter: Vec<CharterEntry> = h
        .http
        .get(format!("{}/charter", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let level = |c: Capability| charter.iter().find(|e| e.capability == c).unwrap();

    assert_eq!(charter.len(), Capability::ALL.len());
    // Already worked before the charter existed; governing must not regress.
    assert_eq!(level(Capability::QueueTasks).level, CharterLevel::Live);
    assert_eq!(level(Capability::DispatchBuilds).level, CharterLevel::Live);
    // Stricter than the status quo, not looser: the alternative is an
    // ungoverned `gh issue create` with no ledger row at all.
    assert_eq!(level(Capability::CaptureWork).level, CharterLevel::Live);
    // Nothing ships rate-limited. Token spend is not a constraint here, and a
    // per-day cap on a *human*-initiated action (cmd-N files through the
    // orchestrator) is incoherent besides.
    assert!(
        charter.iter().all(|e| e.daily_limit.is_none()),
        "no capability should ship with a rate limit"
    );
    // Nothing ships shadowed either. A shadowed capability does the whole job
    // and then hands the result back as prose to be re-entered by hand, which
    // spends more of the human's attention than acting would — and attention,
    // not tokens or nerve, is the scarce thing. What makes `live` safe is the
    // decisions ledger behind it: audit and recourse, not pre-approval.
    assert!(
        charter.iter().all(|e| e.level == CharterLevel::Live),
        "the charter is a kill switch, not a promotion ladder: {charter:?}"
    );
}

/// Off means refused at the endpoint — not discouraged in a prompt the agent
/// may or may not still be weighting.
#[tokio::test]
async fn off_is_enforced() {
    let h = harness().await;
    h.set(Capability::AutoReviewSpecs, CharterLevel::Off, None)
        .await;

    let spec = seed_spec(&h, 810).await;
    let resp = h
        .as_orchestrator(
            h.http
                .post(format!("{}/spec-queue/{}/review", h.base, spec.id)),
        )
        .json(&json!({ "status": "approved", "rationale": "looks fine" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("auto_review_specs"),
        "{body}"
    );

    // Nothing moved, and nothing was recorded — a refusal is not a decision.
    let entry = h
        .store
        .get_spec_queue_entry(&spec.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, SpecQueueStatus::PendingReview);
    assert!(h.store.decisions(None, 10).await.unwrap().is_empty());

    // The human is never gated: they are the accountable party already.
    let resp = h
        .http
        .post(format!("{}/spec-queue/{}/review", h.base, spec.id))
        .json(&json!({ "status": "approved" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Shadow is the whole point of the design: the judgment is real and recorded,
/// the effect never happens, and the response says so rather than looking like
/// success.
#[tokio::test]
async fn shadow_records_the_verdict_and_changes_nothing() {
    let h = harness().await;
    h.set(Capability::AutoReviewSpecs, CharterLevel::Shadow, None)
        .await;
    let spec = seed_spec(&h, 810).await;

    let resp = h
        .as_orchestrator(
            h.http
                .post(format!("{}/spec-queue/{}/review", h.base, spec.id)),
        )
        .json(&json!({
            "status": "approved",
            "rationale": "verification is falsifiable and the base is clean",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["shadowed"], true);
    assert!(
        body["note"].as_str().unwrap().contains("not applied"),
        "{body}"
    );

    // The spec did not move...
    let entry = h
        .store
        .get_spec_queue_entry(&spec.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, SpecQueueStatus::PendingReview);

    // ...but the judgment is on the record, marked as never applied. That
    // distinction is what keeps a shadow run from reading as a history.
    let decisions = h.store.decisions(None, 10).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].action.as_str(), "approve");
    assert_eq!(decisions[0].actor, Actor::Orchestrator);
    assert!(!decisions[0].enforced);
    assert!(
        decisions[0]
            .rationale
            .as_ref()
            .unwrap()
            .contains("falsifiable")
    );
}

/// Live means live — and a shadow decision must not consume the live budget,
/// or an evaluation would run out of room for the thing it is evaluating.
#[tokio::test]
async fn a_daily_budget_bounds_live_actions_only() {
    let h = harness().await;
    h.set(Capability::QueueTasks, CharterLevel::Live, Some(2))
        .await;

    for issue in [1, 2] {
        let task = seed_task(&h, issue, TaskState::Backlog).await;
        let resp = h
            .as_orchestrator(h.http.post(format!("{}/tasks/{}/queue", h.base, task.id)))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "issue {issue} should be within budget");
    }

    let third = seed_task(&h, 3, TaskState::Backlog).await;
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/tasks/{}/queue", h.base, third.id)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("2/day"), "{body}");

    // The task stayed put, and the human can still queue it — a spend cap
    // bounds the orchestrator, not the pipeline.
    let after = h.store.get_task(&third.id).await.unwrap().unwrap();
    assert_eq!(after.state, TaskState::Backlog);
    let resp = h
        .http
        .post(format!("{}/tasks/{}/queue", h.base, third.id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// A capability that could widen its own charter would not be a charter.
#[tokio::test]
async fn the_orchestrator_cannot_set_its_own_charter() {
    let h = harness().await;
    // Demoted first, so the attempt below is a self-promotion — the direction
    // that actually matters. Seeded `live` would make a "live" write a no-op.
    h.set(Capability::RetireWork, CharterLevel::Off, None).await;

    let resp = h
        .as_orchestrator(h.http.post(format!("{}/charter/retire_work", h.base)))
        .json(&json!({ "level": "live" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    // Unchanged by the attempt.
    assert_eq!(
        h.store
            .charter_entry(Capability::RetireWork)
            .await
            .unwrap()
            .level,
        CharterLevel::Off
    );

    // The human sets it, and it takes effect immediately.
    let resp = h
        .http
        .post(format!("{}/charter/retire_work", h.base))
        .json(&json!({ "level": "live", "daily_limit": 3 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let entry: CharterEntry = resp.json().await.unwrap();
    assert_eq!(entry.level, CharterLevel::Live);
    assert_eq!(entry.daily_limit, Some(3));
}

/// Capabilities are independent switches, not one autonomy dial. Dispatching
/// builds while spec review stays in shadow is a coherent — and probably
/// desirable — state, and it is the one a single play/pause toggle cannot
/// express.
#[tokio::test]
async fn capabilities_are_independent() {
    let h = harness().await;
    h.set(Capability::DispatchBuilds, CharterLevel::Live, None)
        .await;
    h.set(Capability::AutoReviewSpecs, CharterLevel::Shadow, None)
        .await;

    let spec = seed_spec(&h, 810).await;
    // Human approves, so the spec is genuinely ready.
    h.http
        .post(format!("{}/spec-queue/{}/review", h.base, spec.id))
        .json(&json!({ "status": "approved" }))
        .send()
        .await
        .unwrap();

    // The orchestrator may dispatch it...
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/builds", h.base)))
        .json(&json!({
            "spec_ids": [spec.id.to_string()],
            "rationale": "only approved spec in the queue; nothing else is in flight",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("shadowed").is_none(), "should be real: {body}");

    // ...while its own verdicts are still only recorded.
    let second = seed_spec(&h, 811).await;
    let resp = h
        .as_orchestrator(
            h.http
                .post(format!("{}/spec-queue/{}/review", h.base, second.id)),
        )
        .json(&json!({ "status": "rejected", "rationale": "duplicates in-flight work" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["shadowed"], true);
}
