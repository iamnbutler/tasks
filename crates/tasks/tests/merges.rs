//! The merge watcher: what happens to a task after its Builder's PR opens.
//!
//! `done` means shipped, so a successful build parks its batch in
//! `awaiting_merge` and the poller resolves the pull request at decision time.
//! GitHub is a real axum server on loopback speaking both halves of what
//! `poll_once` calls — GraphQL for the open-issue set and the close-reason
//! lookup, REST for `GET /pulls/{n}` and the `PATCH /issues/{n}` that closes an
//! issue. The fake is stateful in the one way that matters: an issue we close
//! stops appearing in the open set, exactly as GitHub behaves, so a test can
//! run two polls and watch a merge become `done` through the ordinary
//! closure-derived path.
//!
//! Assertions are on what was *sent* to GitHub as much as on local state — a
//! close that never left the process would pass a store-only test.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Json as AxumJson;
use axum::extract::{Path as AxumPath, State};
use chrono::Utc;
use serde_json::{Value, json};
use tasks::github::{GitHubClient, IntakeFilter};
use tasks::github_health::GitHubHealth;
use tasks::models::{
    Actor, Build, Capability, CharterLevel, Complexity, DecisionAction, DecisionInput, GhState,
    Project, ProjectId, ProjectStatus, Session, SessionId, SessionStatus, Spec, SpecId,
    SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::run::{GitHubWatch, poll_once};
use tasks::store::Store;

/// How the fake's pull request looks, and what it saw.
struct Fake {
    /// The PR body `GET /pulls/{n}` answers with.
    pr: Value,
    /// Issue numbers the repository knows about; a closed one drops out of the
    /// open set, like GitHub.
    issues: Vec<u64>,
    closed: HashSet<u64>,
    /// Every PATCH to `/issues/{n}`.
    patched: Vec<(u64, Value)>,
    /// How many times the PR was read — the ongoing cost this pass pays.
    pr_reads: usize,
    /// What `GET /compare/{base}...{head}` answers with. `identical`/`behind`
    /// mean the head commit is on the base; anything else means it is not.
    compare_status: &'static str,
    /// Every `{base}...{head}` asked about — recording it is what lets a test
    /// assert the *unstacked* case really costs no compare at all.
    compares: Vec<String>,
}

async fn spawn_fake_github(pr: Value, issues: Vec<u64>) -> (String, String, Arc<Mutex<Fake>>) {
    let fake = Arc::new(Mutex::new(Fake {
        pr,
        issues,
        closed: HashSet::new(),
        patched: Vec::new(),
        pr_reads: 0,
        compare_status: "ahead",
        compares: Vec::new(),
    }));
    let app = axum::Router::new()
        .route(
            "/graphql",
            axum::routing::post(
                move |State(f): State<Arc<Mutex<Fake>>>, body: String| async move {
                    let f = f.lock().unwrap();
                    // The close-reason lookup is the only query asking for
                    // `stateReason`; everything else is the open-issue page.
                    if body.contains("stateReason") {
                        let mut nodes = serde_json::Map::new();
                        for number in f.issues.clone() {
                            let closed = f.closed.contains(&number);
                            nodes.insert(
                                format!("i{number}"),
                                json!({
                                    "number": number,
                                    "state": if closed { "CLOSED" } else { "OPEN" },
                                    "stateReason": if closed { json!("COMPLETED") } else { Value::Null },
                                }),
                            );
                        }
                        return AxumJson(json!({ "data": { "repository": nodes } }));
                    }
                    let open: Vec<Value> = f
                        .issues
                        .clone()
                        .into_iter()
                        .filter(|n| !f.closed.contains(n))
                        .map(|n| {
                            json!({
                                "number": n,
                                "title": format!("issue {n}"),
                                "body": "",
                                "state": "OPEN",
                                "updatedAt": Utc::now().to_rfc3339(),
                                "labels": { "nodes": [] },
                            })
                        })
                        .collect();
                    AxumJson(json!({"data": {"repository": {"issues": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": open}}}}))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            axum::routing::get(
                move |State(f): State<Arc<Mutex<Fake>>>,
                      AxumPath((_owner, _repo, _number)): AxumPath<(String, String, u64)>| async move {
                    let mut f = f.lock().unwrap();
                    f.pr_reads += 1;
                    AxumJson(f.pr.clone())
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/compare/{basehead}",
            axum::routing::get(
                move |State(f): State<Arc<Mutex<Fake>>>,
                      AxumPath((_owner, _repo, basehead)): AxumPath<(String, String, String)>| async move {
                    let mut f = f.lock().unwrap();
                    f.compares.push(basehead);
                    AxumJson(json!({ "status": f.compare_status }))
                },
            ),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}",
            axum::routing::patch(
                move |State(f): State<Arc<Mutex<Fake>>>,
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>,
                      AxumJson(body): AxumJson<Value>| async move {
                    let mut f = f.lock().unwrap();
                    f.patched.push((number, body));
                    f.closed.insert(number);
                    AxumJson(json!({ "number": number, "state": "closed" }))
                },
            ),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("{base}/graphql"), base, fake)
}

struct Harness {
    store: Store,
    github: GitHubClient,
    project: Project,
    fake: Arc<Mutex<Fake>>,
}

impl Harness {
    async fn poll(&self) {
        // A throwaway reachability record: this file is about what a pass does
        // with a pull request, not about whether GitHub is answering.
        let health = GitHubHealth::default();
        let watch = GitHubWatch::new(&health, &self.store);
        poll_once(
            &self.store,
            &self.github,
            &IntakeFilter::All,
            "main",
            &watch,
        )
        .await
        .unwrap();
    }

    async fn task(&self, id: &TaskId) -> Task {
        self.store.get_task(id).await.unwrap().unwrap()
    }
}

/// One project, one task parked in `awaiting_merge` behind a succeeded build
/// whose PR is `pr_number`, and a GitHub that answers with `pr`.
async fn harness(issue: u64, pr_number: u64, pr: Value) -> (Harness, Task, Spec, Build) {
    let store = Store::open_in_memory().await.unwrap();
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();

    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: issue,
        title: format!("issue {issue}"),
        body: String::new(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::ReadyToBuild,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
        scout_directions: None,
    };
    store.insert_task(&task).await.unwrap();

    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: None,
        branch: format!("scout/{}", task.id),
        status: SessionStatus::ScoutSucceeded,
        started_at: now,
        completed_at: Some(now),
        exit_reason: None,
        usage: None,
        directions: None,
    };
    store.insert_session(&session).await.unwrap();
    let spec = Spec {
        id: SpecId::new(),
        session_id: Some(session.id),
        task_id: task.id.clone(),
        content: "## Spec".into(),
        complexity: Complexity::Simple,
        files_touched: vec![],
        created_at: now,
    };
    store.insert_spec(&spec).await.unwrap();
    store
        .upsert_spec_queue_entry(&SpecQueueEntry {
            spec_id: spec.id.clone(),
            status: SpecQueueStatus::Approved,
            rank: None,
            approved_at: Some(now),
            feedback: None,
            blocking_dependencies: vec![],
        })
        .await
        .unwrap();

    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();
    let build = store
        .finalize_build_succeeded(&build.id, "headsha", pr_number, None, &[])
        .await
        .unwrap();
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::AwaitingMerge,
        "a build that opened a PR has not shipped anything yet"
    );

    let (graphql, rest, fake) = spawn_fake_github(pr, vec![issue]).await;
    let github = GitHubClient::with_base_url("token", graphql).with_rest_base_url(rest);
    (
        Harness {
            store,
            github,
            project,
            fake,
        },
        task,
        spec,
        build,
    )
}

/// The ordinary case: merged straight into the trunk.
fn merged_pr(number: u64) -> Value {
    merged_pr_into(number, "main")
}

/// Merged into `base`. When `base` is not the trunk this is a **stacked** PR:
/// `merged` is true and nothing has necessarily shipped.
fn merged_pr_into(number: u64, base: &str) -> Value {
    json!({
        "number": number,
        "state": "closed",
        "merged": true,
        "mergeable": Value::Null,
        "merge_commit_sha": "cafef00d",
        "base": { "ref": base },
    })
}

/// The whole happy path, across the two polls it really takes: the first sees
/// the merge and closes the issue upstream, the second observes the closure and
/// retires the task through the ordinary closure-derived path. `done` is
/// written in exactly one place, and it means the issue is closed.
#[tokio::test]
async fn a_merged_pull_request_closes_the_issue_and_the_next_poll_retires_the_task() {
    let (h, task, _spec, build) = harness(41, 900, merged_pr(900)).await;

    h.poll().await;

    {
        let fake = h.fake.lock().unwrap();
        assert_eq!(fake.pr_reads, 1, "one REST call per unresolved PR per poll");
        assert_eq!(fake.patched.len(), 1, "one close, for the one issue");
        let (number, body) = &fake.patched[0];
        assert_eq!(*number, 41);
        assert_eq!(body["state"], "closed");
        assert_eq!(
            body["state_reason"], "completed",
            "a merged PR is evidence the work was completed"
        );
    }
    assert_eq!(
        h.task(&task.id).await.state,
        TaskState::AwaitingMerge,
        "closure is GitHub's fact; nothing is marked done in anticipation"
    );

    // The ledger row carries the merge commit, and is the server's own —
    // never the human's, who is the one actor the charter never gates.
    let ledger = h
        .store
        .decisions(Some(("task", task.id.as_str())), 10)
        .await
        .unwrap();
    let closed = ledger
        .iter()
        .find(|d| d.action == DecisionAction::RetireWork)
        .expect("the close is in the ledger");
    assert_eq!(closed.actor, Actor::System);
    assert!(closed.enforced);
    let evidence = closed.evidence.clone().unwrap();
    assert_eq!(evidence["pr_number"], 900);
    assert_eq!(evidence["merge_commit_sha"], "cafef00d");
    assert_eq!(evidence["build_id"], build.id.as_str());

    h.poll().await;

    let after = h.task(&task.id).await;
    assert_eq!(after.gh_state, GhState::Closed);
    assert_eq!(after.state, TaskState::Done, "done means shipped");
    assert_eq!(
        h.fake.lock().unwrap().patched.len(),
        1,
        "the second poll must not re-close what it just closed"
    );
    assert_eq!(
        h.fake.lock().unwrap().pr_reads,
        1,
        "and it stops paying for the PR read once the work is retired"
    );
}

/// The branch is not going to land: the batch goes back on the shelf with a
/// strike charged, and nothing is closed. Returning the task to
/// `ready_to_build` restores the *option* to rebuild — builds are never
/// dispatched automatically — so there is no rebuild loop here.
#[tokio::test]
async fn a_pull_request_closed_unmerged_returns_the_batch_to_ready_to_build() {
    let (h, task, spec, build) = harness(42, 901, {
        json!({
            "number": 901,
            "state": "closed",
            "merged": false,
            "mergeable": Value::Null,
            "merge_commit_sha": Value::Null,
        })
    })
    .await;

    h.poll().await;

    assert_eq!(h.task(&task.id).await.state, TaskState::ReadyToBuild);
    let entry = h
        .store
        .get_spec_queue_entry(&spec.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, SpecQueueStatus::Approved, "buildable again");
    assert_eq!(
        h.store.get_build(&build.id).await.unwrap().unwrap().status,
        tasks::models::BuildStatus::Succeeded,
        "the build row is history: it did push a branch and open a PR"
    );
    assert!(
        h.fake.lock().unwrap().patched.is_empty(),
        "nothing shipped, so nothing is closed"
    );
}

/// GitHub populates `merge_commit_sha` on *open* PRs too, from its speculative
/// test merge. A reader that treats it as evidence closes issues for work that
/// never landed.
#[tokio::test]
async fn an_open_pull_request_with_a_speculative_merge_sha_closes_nothing() {
    let (h, task, _spec, _build) = harness(43, 902, {
        json!({
            "number": 902,
            "state": "open",
            "merged": false,
            "mergeable": true,
            "merge_commit_sha": "5peculat1ve",
        })
    })
    .await;

    h.poll().await;
    h.poll().await;

    assert_eq!(h.task(&task.id).await.state, TaskState::AwaitingMerge);
    assert!(h.fake.lock().unwrap().patched.is_empty());
    assert_eq!(
        h.fake.lock().unwrap().pr_reads,
        2,
        "an unresolved PR is re-read every poll; the answer is never stored"
    );
}

/// `retire_work: off` is a kill switch, and it costs an API call to honour.
#[tokio::test]
async fn the_charter_can_switch_the_whole_pass_off() {
    let (h, task, _spec, _build) = harness(44, 903, merged_pr(903)).await;
    h.store
        .set_charter(Capability::RetireWork, CharterLevel::Off, None)
        .await
        .unwrap();

    h.poll().await;

    assert_eq!(h.task(&task.id).await.state, TaskState::AwaitingMerge);
    let fake = h.fake.lock().unwrap();
    assert!(fake.patched.is_empty());
    assert_eq!(fake.pr_reads, 0, "off spends nothing, not even a read");
}

/// Shadow records the judgment and applies nothing — and because a shadowed
/// close changes nothing, the same build is on the list at the next poll. One
/// ledger row, not one a minute.
#[tokio::test]
async fn shadow_records_the_close_once_and_applies_nothing() {
    let (h, task, _spec, _build) = harness(45, 904, merged_pr(904)).await;
    h.store
        .set_charter(Capability::RetireWork, CharterLevel::Shadow, None)
        .await
        .unwrap();

    h.poll().await;
    h.poll().await;

    assert_eq!(h.task(&task.id).await.state, TaskState::AwaitingMerge);
    assert!(h.fake.lock().unwrap().patched.is_empty());

    let shadowed: Vec<_> = h
        .store
        .decisions(Some(("task", task.id.as_str())), 20)
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.action == DecisionAction::RetireWork)
        .collect();
    assert_eq!(shadowed.len(), 1, "deduped across polls");
    assert!(!shadowed[0].enforced, "recorded, never applied");
    assert_eq!(shadowed[0].actor, Actor::System);
    assert_eq!(
        h.store.list_projects().await.unwrap()[0].id,
        h.project.id,
        "sanity: the harness polled the project under test"
    );
}

/// The unstacked case is free. `base.ref == trunk` answers the whole question
/// on its own, so the compare is never spent — which is what makes reading
/// reachability affordable on every poll.
#[tokio::test]
async fn a_pull_request_based_on_the_trunk_costs_no_compare() {
    let (h, task, _spec, _build) = harness(46, 905, merged_pr_into(905, "main")).await;

    h.poll().await;

    assert!(
        h.fake.lock().unwrap().compares.is_empty(),
        "base is the trunk; there is nothing left to ask GitHub"
    );
    assert_eq!(h.fake.lock().unwrap().patched.len(), 1, "and it shipped");
    h.poll().await;
    assert_eq!(h.task(&task.id).await.state, TaskState::Done);
}

/// **The bug this pass exists to stop reintroducing.** A PR stacked on another
/// build's branch reads `merged: true` the moment that branch takes it. This is
/// how PR #863 itself was lost — merged, and on no branch that ships.
///
/// The batch stays parked, is *not* unwound (a merged PR is also a closed one,
/// and unwinding here would rebuild work sitting in a legitimate stack), and
/// nothing is closed.
#[tokio::test]
async fn a_merged_pull_request_whose_commit_never_reached_the_trunk_ships_nothing() {
    let (h, task, spec, _build) = harness(47, 906, merged_pr_into(906, "build/underneath")).await;
    h.fake.lock().unwrap().compare_status = "ahead";

    h.poll().await;
    h.poll().await;

    assert_eq!(
        h.task(&task.id).await.state,
        TaskState::AwaitingMerge,
        "merged into a branch that has not landed is not shipped"
    );
    assert!(
        h.fake.lock().unwrap().patched.is_empty(),
        "closing the issue here would be #859's failure one level up"
    );
    assert_eq!(
        h.store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Built,
        "and it is not unwound either: the stack may still land"
    );
    let fake = h.fake.lock().unwrap();
    assert_eq!(fake.compares.len(), 2, "asked again on the second poll");
    assert_eq!(
        fake.compares[0], "main...cafef00d",
        "compare reads head relative to base — reversing this inverts the verdict"
    );
}

/// The other stack order: the dependent merged first, and its base lands
/// afterwards. Reachability is monotone, so the later poll simply finds the
/// commit on the trunk and closes normally. Nothing had to be un-done.
#[tokio::test]
async fn a_stacked_batch_ships_once_its_base_reaches_the_trunk() {
    let (h, task, _spec, _build) = harness(48, 907, merged_pr_into(907, "build/underneath")).await;
    h.fake.lock().unwrap().compare_status = "ahead";

    h.poll().await;
    assert_eq!(h.task(&task.id).await.state, TaskState::AwaitingMerge);
    assert!(h.fake.lock().unwrap().patched.is_empty());

    // The base merges; the merge commit is now an ancestor of the trunk.
    h.fake.lock().unwrap().compare_status = "behind";

    h.poll().await;
    assert_eq!(h.fake.lock().unwrap().patched.len(), 1, "now it shipped");
    h.poll().await;
    assert_eq!(h.task(&task.id).await.state, TaskState::Done);
}

/// Every unreadable answer is "not yet". Saying so costs one call on the next
/// poll; saying "shipped" wrongly writes `done` over work that shipped nothing,
/// and no pass ever revisits `done`.
#[tokio::test]
async fn a_merge_with_no_commit_to_check_stays_parked() {
    let (h, task, _spec, _build) = harness(49, 908, {
        json!({
            "number": 908,
            "state": "closed",
            "merged": true,
            "mergeable": Value::Null,
            "merge_commit_sha": Value::Null,
            "base": { "ref": "build/underneath" },
        })
    })
    .await;

    h.poll().await;

    assert_eq!(h.task(&task.id).await.state, TaskState::AwaitingMerge);
    assert!(h.fake.lock().unwrap().patched.is_empty());
    assert!(
        h.fake.lock().unwrap().compares.is_empty(),
        "there was no sha to compare"
    );
}

/// A batch nobody lands keeps raising `land_batch`, which is the only thing in
/// the system that notices stranding — and its subject is a **build** id.
#[tokio::test]
async fn a_stranded_batch_raises_a_standing_obligation() {
    let (h, _task, _spec, build) = harness(50, 909, merged_pr_into(909, "build/underneath")).await;
    h.fake.lock().unwrap().compare_status = "ahead";

    h.poll().await;

    let obligations = h
        .store
        .open_obligations(chrono::Duration::zero())
        .await
        .unwrap();
    let landing: Vec<_> = obligations
        .iter()
        .filter(|o| o.kind == tasks::models::ObligationKind::LandBatch)
        .collect();
    assert_eq!(landing.len(), 1);
    assert_eq!(
        landing[0].subject_id,
        build.id.as_str(),
        "keyed to the build: a batch ships or strands together"
    );
    assert!(
        landing[0].summary.contains("#909"),
        "names the PR that has to land: {}",
        landing[0].summary
    );
}
