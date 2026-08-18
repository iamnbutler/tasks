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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::Json as AxumJson;
use axum::extract::{Path as AxumPath, State};
use chrono::Utc;
use serde_json::{Value, json};
use tasks::github::{GitHubClient, IntakeFilter};
use tasks::github_health::GitHubHealth;
use tasks::models::{
    Actor, Build, Capability, CharterLevel, Complexity, DecisionAction, DecisionInput,
    DecisionState, GhState, Project, ProjectId, ProjectStatus, Session, SessionId, SessionStatus,
    Spec, SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::run::{GitHubWatch, poll_once};
use tasks::store::Store;

/// How the fake's pull request looks, and what it saw.
struct Fake {
    /// The PR body `GET /pulls/{n}` answers with, for any number with no
    /// entry in `prs`.
    pr: Value,
    /// Per-number PR bodies, overlaying `pr`. A pipeline that rebuilds a spec
    /// has *two* PRs with different endings — one closed unmerged, one open —
    /// and a fake serving one body for every number cannot express that at
    /// all, which is the shape of #956/#959.
    prs: HashMap<u64, Value>,
    /// Which PR numbers were read, in order. What lets a test assert that a
    /// settled PR is not being re-read every poll.
    read_numbers: Vec<u64>,
    /// Issue numbers the repository knows about; a closed one drops out of the
    /// open set, like GitHub.
    issues: Vec<u64>,
    closed: HashSet<u64>,
    /// Every PATCH to `/issues/{n}`.
    patched: Vec<(u64, Value)>,
    /// What `PATCH /issues/{n}` answers with. 503 is GitHub *not answering*,
    /// which is the branch that must leave the decision pending.
    close_status: u16,
    /// Pending decision rows at the instant each close request arrived. The
    /// fake runs in this process, so it can read the ledger — which is the
    /// only place the *ordering* of intent and effect is observable.
    pending_at_close: Vec<usize>,
    /// How many times the PR was read — the ongoing cost this pass pays.
    pr_reads: usize,
    /// What `GET /compare/{base}...{head}` answers with. `identical`/`behind`
    /// mean the head commit is on the base; anything else means it is not.
    compare_status: &'static str,
    /// Every `{base}...{head}` asked about — recording it is what lets a test
    /// assert the *unstacked* case really costs no compare at all.
    compares: Vec<String>,
}

/// As [`spawn_fake_github`], plus a store the fake reads the pending-decision
/// count out of when a close arrives.
async fn spawn_fake_github_watching(
    pr: Value,
    issues: Vec<u64>,
    ledger: Option<Arc<Store>>,
) -> (String, String, Arc<Mutex<Fake>>) {
    let fake = Arc::new(Mutex::new(Fake {
        pr,
        prs: HashMap::new(),
        read_numbers: Vec::new(),
        issues,
        closed: HashSet::new(),
        patched: Vec::new(),
        close_status: 200,
        pending_at_close: Vec::new(),
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
                      AxumPath((_owner, _repo, number)): AxumPath<(String, String, u64)>| async move {
                    let mut f = f.lock().unwrap();
                    f.pr_reads += 1;
                    f.read_numbers.push(number);
                    let body = f.prs.get(&number).cloned().unwrap_or_else(|| f.pr.clone());
                    AxumJson(body)
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
                      AxumJson(body): AxumJson<Value>| {
                    let ledger = ledger.clone();
                    async move {
                        let pending = match &ledger {
                            Some(store) => store.pending_decisions().await.unwrap().len(),
                            None => 0,
                        };
                        let status = {
                            let mut f = f.lock().unwrap();
                            f.pending_at_close.push(pending);
                            f.patched.push((number, body));
                            let status = f.close_status;
                            if status == 200 {
                                f.closed.insert(number);
                            }
                            status
                        };
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            AxumJson(json!({ "number": number, "state": "closed" })),
                        )
                    }
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
    store: Arc<Store>,
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
    let store = Arc::new(Store::open_in_memory().await.unwrap());
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

    // The fake reads this store's ledger when a close arrives — see
    // `a_close_records_its_intent_before_it_reaches_github`.
    let (graphql, rest, fake) =
        spawn_fake_github_watching(pr, vec![issue], Some(store.clone())).await;
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

/// The same story as `store::tests::a_rebuilt_batch_stops_obligating_…`, but
/// through `poll_once` against a GitHub holding both pull requests — because
/// the poller half is the destructive one.
///
/// PR 946 closed unmerged, the spec was rebuilt, PR 952 is open. The dead
/// build must be nobody's business: not re-read, not unwound, and not raising
/// an obligation nothing could discharge. Left unfixed, every poll would read
/// 946, find it closed-unmerged forever, charge the *live* build's spec a
/// build attempt, and pull #938 out of `awaiting_merge` while 952 was open.
#[tokio::test]
async fn a_rebuilt_batch_leaves_the_build_it_was_rebuilt_past_alone() {
    // The harness parks the task behind PR 946 for us; the fake answers with
    // 946's ending until the second PR is registered below.
    let (h, task, spec, dead) = harness(938, 946, {
        json!({
            "number": 946,
            "state": "closed",
            "merged": false,
            "mergeable": Value::Null,
            "merge_commit_sha": Value::Null,
        })
    })
    .await;

    // Poll one: the genuine verdict on 946. The batch comes back.
    h.poll().await;
    assert_eq!(h.task(&task.id).await.state, TaskState::ReadyToBuild);

    // The rebuild, parking the same task behind PR 952 — which is open.
    let live = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    h.store.claim_next_queued_build().await.unwrap().unwrap();
    h.store
        .finalize_build_succeeded(&live.id, "headsha2", 952, None, &[])
        .await
        .unwrap();
    {
        let mut fake = h.fake.lock().unwrap();
        fake.prs.insert(
            952,
            json!({
                "number": 952,
                "state": "open",
                "merged": false,
                "mergeable": true,
                "merge_commit_sha": Value::Null,
            }),
        );
        fake.read_numbers.clear();
    }

    h.poll().await;

    assert_eq!(
        h.fake.lock().unwrap().read_numbers,
        vec![952],
        "946 is settled and belongs to a build nothing owns any more"
    );
    assert_eq!(
        h.task(&task.id).await.state,
        TaskState::AwaitingMerge,
        "PR 952 is open; nothing may drag the task out from under it"
    );
    assert_eq!(
        h.store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Built,
        "and the live build's spec is not charged for the dead one's PR"
    );

    let landing: Vec<_> = h
        .store
        .open_obligations(chrono::Duration::zero())
        .await
        .unwrap()
        .into_iter()
        .filter(|o| o.kind == tasks::models::ObligationKind::LandBatch)
        .collect();
    assert_eq!(landing.len(), 1, "{landing:?}");
    assert_eq!(
        landing[0].subject_id,
        live.id.to_string(),
        "an obligation naming PR 946 is one no act could ever discharge (#956)"
    );
    assert_ne!(landing[0].subject_id, dead.id.to_string());
}

/// `watch_merges` is the one GitHub write outside the nine handlers, and no
/// route guard reaches it: `tests/custodial.rs`'s
/// `no_write_route_reaches_github_without_recording_first` drives routes, and
/// this is a poll. So it gets its own.
///
/// Three things, in one story. The intent is on record **before** the close
/// leaves the process — the fake reads the ledger as the request arrives,
/// which is the only place ordering is observable. A close GitHub never
/// answers leaves the row `pending`, because a write that may have landed must
/// not be recorded as one that did not. And the retry reuses that same intent
/// rather than writing a second one, because there is only one intent and a
/// poll every minute through an outage would otherwise leave a row a minute.
#[tokio::test]
async fn a_close_records_its_intent_before_it_reaches_github() {
    let (h, task, _spec, _build) = harness(51, 910, merged_pr(910)).await;
    h.fake.lock().unwrap().close_status = 503;

    h.poll().await;

    {
        let fake = h.fake.lock().unwrap();
        assert_eq!(fake.patched.len(), 1, "the close was attempted");
        assert_eq!(
            fake.pending_at_close,
            vec![1],
            "and its intent was already in the ledger when it landed"
        );
    }
    let pending = h.store.pending_decisions().await.unwrap();
    assert_eq!(pending.len(), 1, "GitHub never answered, so nobody knows");
    assert_eq!(pending[0].action, DecisionAction::RetireWork);
    assert_eq!(pending[0].actor, Actor::System);
    assert_eq!(pending[0].outcome.as_ref().unwrap()["intent"]["issue"], 51);
    assert!(
        pending[0].outcome.as_ref().unwrap()["unanswered"]
            .as_str()
            .unwrap()
            .contains("503"),
        "{:?}",
        pending[0].outcome
    );
    assert_eq!(
        h.task(&task.id).await.state,
        TaskState::AwaitingMerge,
        "a pending close is neither a failure nor a success: the task stays parked \
         and the next poll asks again"
    );

    // The next poll retries the close under the same intent rather than
    // opening a second one.
    h.poll().await;
    assert_eq!(h.fake.lock().unwrap().patched.len(), 2, "retried");
    assert_eq!(
        h.store.pending_decisions().await.unwrap().len(),
        1,
        "one intent, however many attempts it takes"
    );

    // And when GitHub finally answers, that same row settles.
    h.fake.lock().unwrap().close_status = 200;
    h.poll().await;
    assert!(
        h.store.pending_decisions().await.unwrap().is_empty(),
        "the window closes when the answer arrives"
    );
    let settled = h
        .store
        .decisions(Some(("task", task.id.as_str())), 10)
        .await
        .unwrap();
    let retire = settled
        .iter()
        .find(|d| d.action == DecisionAction::RetireWork)
        .expect("one row, start to finish");
    assert_eq!(retire.state, DecisionState::Applied);
    assert_eq!(retire.outcome.as_ref().unwrap()["closed"], 51);
    assert_eq!(
        settled
            .iter()
            .filter(|d| d.action == DecisionAction::RetireWork)
            .count(),
        1,
        "and exactly one, across three polls"
    );
}

/// GitHub *answered* the close with a 4xx — nothing reached the world — so the
/// intent is annulled rather than left open. `pending` means nobody knows, and
/// conflating the two is what the whole state column exists to prevent.
#[tokio::test]
async fn a_close_github_refuses_is_annulled_rather_than_left_pending() {
    let (h, task, _spec, _build) = harness(52, 911, merged_pr(911)).await;
    h.fake.lock().unwrap().close_status = 422;

    h.poll().await;

    assert!(h.store.pending_decisions().await.unwrap().is_empty());
    let row = h
        .store
        .decisions(Some(("task", task.id.as_str())), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.action == DecisionAction::RetireWork)
        .expect("the judgment is on record either way");
    assert_eq!(row.state, DecisionState::Annulled);
    assert!(row.outcome.as_ref().unwrap()["refused"].is_string());
    assert_eq!(
        h.task(&task.id).await.state,
        TaskState::AwaitingMerge,
        "nothing shipped, so nothing moves"
    );
}

/// #956's parked-rather-than-unwound rule survives the restructuring: a batch
/// that merged but has not reached the trunk stays parked, and the close it
/// never made leaves no ledger row at all.
///
/// Stated as its own test because the resolution recording is now wrapped
/// around exactly this branch, and "it must not become an unwind" is the kind
/// of invariant a refactor takes out silently.
#[tokio::test]
async fn a_merged_but_unreachable_batch_stays_parked_and_records_no_close() {
    let (h, task, spec, _build) = harness(53, 912, merged_pr_into(912, "build/underneath")).await;
    h.fake.lock().unwrap().compare_status = "ahead";

    h.poll().await;
    h.poll().await;

    assert_eq!(h.task(&task.id).await.state, TaskState::AwaitingMerge);
    assert_eq!(
        h.store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Built,
        "the stack may still land; unwinding here would rebuild work that is sitting in it"
    );
    assert!(
        h.store
            .decisions(None, 20)
            .await
            .unwrap()
            .iter()
            .all(|d| d.action != DecisionAction::RetireWork),
        "no close was made, so there is no close to explain"
    );
    assert!(h.store.pending_decisions().await.unwrap().is_empty());
}
