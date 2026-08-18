//! End-to-end tests for the server loops in [`tasks::run`].
//!
//! The poll tests run against a local axum server returning canned GraphQL
//! payloads; the dispatch tests run against a real vm-pool service driving the
//! real scout-supervisor binary with a stub agent. No mocks.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Json as AxumJson};
use axum::routing::post;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::watch;
use vm_pool_client::Client;
use vm_pool_manager::SupervisorRuntime;
use vm_pool_protocol::VmConfig;
use vm_pool_service::Service;

use tasks_protocol::TasksProtocol;

use tasks::events::EventPayload;
use tasks::github::{GitHubClient, IntakeFilter};
use tasks::github_health::GitHubHealth;
use tasks::models::{
    Build, BuildStatus, Complexity, DecisionInput, GhState, Mode, Project, ProjectId,
    ProjectStatus, Session, SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry,
    SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::pool_health::PoolHealth;
use tasks::run::{self, Config, GitHubWatch, InFlight};
use tasks::store::Store;
use tasks::updates::UpdateWatch;

mod common;
use common::{
    api_death_agent_path, make_fixture_repo, spawn_vm_pool, stub_agent_path, wait_until,
    workspace_bin, write_supervisor_wrapper_with_env,
};

/// An enforcing update watch with nothing pending: the test binary's mtime
/// predates the process running it, and a fresh store has observed no images.
fn test_update_watch() -> Arc<UpdateWatch> {
    Arc::new(UpdateWatch::at_boot(true))
}

/// A fresh vm-pool capacity record. It has observed nothing, so it holds
/// nothing — the loop's own first probe is what fills it in.
fn test_pool_health() -> Arc<PoolHealth> {
    Arc::new(PoolHealth::new())
}

// --- GitHub poll loop ---

/// Serve canned GraphQL responses on loopback. Each POST pops the next
/// response; once the queue is down to its last entry that entry repeats, so a
/// polling loop keeps seeing a stable repository.
async fn spawn_fake_github(responses: Vec<Value>) -> String {
    assert!(!responses.is_empty(), "need at least one canned response");
    let queue = Arc::new(Mutex::new(responses));
    let app = Router::new()
        .route(
            "/graphql",
            post(
                move |State(q): State<Arc<Mutex<Vec<Value>>>>, _body: String| async move {
                    let resp = {
                        let mut g = q.lock().unwrap();
                        if g.len() > 1 {
                            g.remove(0)
                        } else {
                            g[0].clone()
                        }
                    };
                    AxumJson(resp)
                },
            ),
        )
        .with_state(queue);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/graphql")
}

fn page(nodes: Vec<Value>) -> Value {
    json!({
        "data": {
            "repository": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": nodes,
                }
            }
        }
    })
}

fn issue(number: u64, title: &str, state: &str) -> Value {
    labelled_issue(number, title, state, &[])
}

fn labelled_issue(number: u64, title: &str, state: &str, labels: &[&str]) -> Value {
    json!({
        "number": number,
        "title": title,
        "body": format!("body of {number}"),
        "state": state,
        "updatedAt": "2026-08-09T00:00:00Z",
        "labels": {
            "nodes": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
        },
    })
}

async fn insert_project(store: &Store) -> Project {
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();
    project
}

/// [`run::poll_once`] with a throwaway reachability record.
///
/// Most of this file is about what a pass *does*; the tests that are about
/// whether GitHub is answering build their own [`GitHubHealth`] and read it
/// back. One wrapper keeps that distinction to the tests that mean it.
async fn poll_once(
    store: &Store,
    github: &GitHubClient,
    intake: &IntakeFilter,
    trunk: &str,
) -> Result<usize, tasks::store::StoreError> {
    let health = GitHubHealth::default();
    let watch = GitHubWatch::new(&health, store);
    run::poll_once(store, github, intake, trunk, &watch).await
}

// --- GitHub reachability ---

/// A GraphQL endpoint that can be taken down and brought back while a poll loop
/// is running: while `down`, every request is a real 503 off a real server.
async fn spawn_switchable_github(page: Value) -> (String, Arc<AtomicBool>) {
    let down = Arc::new(AtomicBool::new(true));
    let state = (down.clone(), page);
    let app = Router::new()
        .route(
            "/graphql",
            post(
                move |State((down, page)): State<(Arc<AtomicBool>, Value)>,
                      _body: String| async move {
                    if down.load(Ordering::SeqCst) {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            AxumJson(json!({"message": "Service Unavailable"})),
                        )
                            .into_response();
                    }
                    AxumJson(page.clone()).into_response()
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/graphql"), down)
}

/// Notes the poller wrote about GitHub's reachability, in order.
async fn poller_notes(store: &Store) -> Vec<String> {
    store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::Note { source, message } if source == "poller" => Some(message),
            _ => None,
        })
        .collect()
}

/// An outage nobody could have prevented, set by hand — the state a poll loop
/// would have left behind.
fn unavailable() -> Result<(), tasks::github::GhError> {
    Err(tasks::github::GhError::Rest {
        what: "list issues".into(),
        status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
        message: "Service Unavailable".into(),
    })
}

/// The whole reachability record, driven by a real poll loop against a real
/// server that stops answering and then starts again.
///
/// Three things at once, because they are the three rules: one failed call
/// holds, the hold is announced exactly *once* however long the outage runs,
/// and only a success releases it — after which intake resumes on its own.
#[tokio::test]
async fn a_failing_poll_holds_dispatch_announces_once_and_releases_on_recovery() {
    let (github_url, down) = spawn_switchable_github(page(vec![issue(1, "first", "OPEN")])).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    insert_project(&store).await;
    store.set_mode(Mode::Play).await.unwrap();

    let mut config = test_config(Path::new("/nonexistent"), Path::new("/nonexistent"), 1);
    config.github_token = Some("token".into());
    config.github_api_url = Some(github_url);
    config.poll_interval = Duration::from_millis(50);

    let health = Arc::new(GitHubHealth::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::poll_loop(
        store.clone(),
        config,
        health.clone(),
        shutdown_rx,
    ));

    let h = health.clone();
    wait_until(Duration::from_secs(10), || {
        let h = h.clone();
        async move { h.hold(Utc::now()).is_some() }
    })
    .await;
    // Several more failed passes: the hold keeps counting, and stays quiet.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let held = health.hold(Utc::now()).expect("still held");
    assert!(
        held.failures > 1,
        "the poller keeps looking: {} failures",
        held.failures
    );
    assert!(store.list_tasks().await.unwrap().is_empty());
    let notes = poller_notes(&store).await;
    assert_eq!(notes.len(), 1, "one edge, one announcement: {notes:?}");
    assert!(notes[0].contains("not answering"), "{}", notes[0]);
    assert!(
        notes[0].contains("nothing is charged an attempt"),
        "{}",
        notes[0]
    );

    // The release half. Without it, "held ⇒ nothing ingested" passes just as
    // well when the loop is simply broken.
    down.store(false, Ordering::SeqCst);
    let s = store.clone();
    wait_until(Duration::from_secs(10), || {
        let s = s.clone();
        async move { !s.list_tasks().await.unwrap().is_empty() }
    })
    .await;
    assert!(health.hold(Utc::now()).is_none(), "a success releases it");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let notes = poller_notes(&store).await;
    assert_eq!(notes.len(), 2, "the release is one edge too: {notes:?}");
    assert!(notes[1].contains("answering again"), "{}", notes[1]);

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("poll loop exits on shutdown")
        .unwrap();
}

/// A Scout's first act is a clone, so a scout started during an outage dies in
/// setup and is charged a dispatch attempt for something no task did (#939).
///
/// The release half is not optional: "held ⇒ nothing dispatched" passes just as
/// well when the dispatch loop is broken, so the test flips the record back and
/// waits for the work to actually go out.
#[tokio::test]
async fn a_github_hold_starts_no_scout_and_charges_nothing() {
    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "waits for github").await;
    store.set_mode(Mode::Play).await.unwrap();

    let health = Arc::new(GitHubHealth::default());
    health.observe(&unavailable(), Utc::now());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        health.clone(),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        store.list_sessions().await.unwrap().is_empty(),
        "a held dispatcher starts nothing"
    );
    let held = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(held.state, TaskState::Queued, "and moves nothing");
    assert_eq!(
        held.dispatch_attempts, 0,
        "holding costs the task no attempt — that is the whole point"
    );

    health.observe(&Ok::<(), tasks::github::GhError>(()), Utc::now());
    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 1 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    assert_eq!(dispatch_order(&store).await, vec![task.id.clone()]);
    assert_eq!(
        store
            .get_task(&task.id)
            .await
            .unwrap()
            .unwrap()
            .dispatch_attempts,
        0
    );
}

/// The serial build lane, which clones too — and where the bug was expensive:
/// one outage charged a build attempt to every spec in the batch.
///
/// The hold sits in the loop's match guard, *ahead* of the claim, so a held
/// lane must leave the build `queued` rather than flipping it `running` (and
/// its batch's tasks to `building`) on every tick of the outage.
#[tokio::test]
async fn a_github_hold_never_claims_a_build() {
    let (_tmp, store, mut config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let (task, build) = queued_build(&store, &project).await;
    store.set_mode(Mode::Play).await.unwrap();

    // An unconfigured lane disables itself, and this test would then pass for
    // the wrong reason. The budget is short because the release half really
    // dispatches, and `build_loop` awaits a build inline.
    config.github_token = Some("token".into());
    config.builder_timeout = Duration::from_secs(10);

    let health = Arc::new(GitHubHealth::default());
    health.observe(&unavailable(), Utc::now());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::build_loop(
        store.clone(),
        config,
        InFlight::default(),
        health.clone(),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        store.get_build(&build.id).await.unwrap().unwrap().status,
        BuildStatus::Queued,
        "a held lane never claims"
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild,
        "and never drags its batch's tasks to `building`"
    );

    health.observe(&Ok::<(), tasks::github::GhError>(()), Utc::now());
    let s = store.clone();
    let id = build.id.clone();
    wait_until(Duration::from_secs(30), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_build(&id).await.unwrap().unwrap().status != BuildStatus::Queued }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("build loop exits on shutdown")
        .unwrap();
}

/// #967, the scout half. A pool with no free slot must **hold** dispatch, not
/// dispatch into a refusal.
///
/// Disabling this gate alone (leaving the unwind) is what makes the point:
/// `DISPATCH_TICK` is 500ms, so the loop burns all three `dispatch_attempts` in
/// under two seconds and rejects a healthy task — `Rejected` where this asserts
/// `Queued`. With #930 in force it does not reject, and instead produces an
/// unbounded stream of waiver `Note`s into the feed and the orchestrator's
/// input, which is the same failure one level up.
///
/// The release half is not optional: "held ⇒ nothing dispatched" passes just as
/// well when the dispatch loop is broken. So the test hands the slot back and
/// waits for the work to actually go out. Both halves over one real pool, no
/// mocks.
#[tokio::test]
async fn a_full_pool_starts_no_scout_and_charges_nothing() {
    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "waits for a slot").await;
    store.set_mode(Mode::Play).await.unwrap();

    // Take the pool's only slot, from outside the loop under test.
    let squatter_client: Client<TasksProtocol> =
        Client::connect(&config.vm_pool_socket).await.unwrap();
    let squatter = squatter_client
        .handle()
        .allocate("agent:v1", VmConfig::default())
        .await
        .expect("the only slot");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        store.list_sessions().await.unwrap().is_empty(),
        "a held dispatcher starts nothing"
    );
    let held = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(
        held.state,
        TaskState::Queued,
        "and never claims a task it cannot start"
    );
    assert_eq!(
        held.dispatch_attempts, 0,
        "holding costs the task no attempt — that is the whole point"
    );

    // The hold is announced once, and it says what it costs.
    let notes = dispatcher_notes(&store).await;
    assert_eq!(notes.len(), 1, "one edge, one note: {notes:?}");
    assert!(notes[0].contains("no free slot"), "{}", notes[0]);
    assert!(
        notes[0].contains("nothing is charged an attempt"),
        "{}",
        notes[0]
    );

    // Release: the same task, from the same state, scouts to a spec.
    squatter_client
        .handle()
        .deallocate(&squatter)
        .await
        .unwrap();
    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 1 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    assert_eq!(
        store
            .get_task(&task.id)
            .await
            .unwrap()
            .unwrap()
            .dispatch_attempts,
        0,
        "and it never cost an attempt along the way"
    );
}

/// #967, the build half. The hold sits in the match guard **ahead of the
/// claim**, for the same reason the GitHub one does: claiming flips the build
/// `queued → running` and drags its batch's tasks to `building`, so a
/// claim-then-refuse would do that on every tick of a full pool.
///
/// Disabling this gate alone gives `Failed` where this asserts `Queued`.
#[tokio::test]
async fn a_full_pool_never_claims_a_build() {
    let (_tmp, store, mut config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let (task, build) = queued_build(&store, &project).await;
    store.set_mode(Mode::Play).await.unwrap();

    // An unconfigured lane disables itself, and this test would then pass for
    // the wrong reason.
    config.github_token = Some("token".into());
    config.builder_timeout = Duration::from_secs(10);

    let squatter_client: Client<TasksProtocol> =
        Client::connect(&config.vm_pool_socket).await.unwrap();
    let squatter = squatter_client
        .handle()
        .allocate("agent:v1", VmConfig::default())
        .await
        .expect("the only slot");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::build_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        store.get_build(&build.id).await.unwrap().unwrap().status,
        BuildStatus::Queued,
        "a held lane never claims"
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild,
        "and never drags its batch's tasks to `building`"
    );

    squatter_client
        .handle()
        .deallocate(&squatter)
        .await
        .unwrap();
    let s = store.clone();
    let id = build.id.clone();
    wait_until(Duration::from_secs(30), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_build(&id).await.unwrap().unwrap().status != BuildStatus::Queued }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("build loop exits on shutdown")
        .unwrap();
}

/// Two loops racing across one edge write **one** `Note`, not two — the probe
/// claim is what admits exactly one of them to the `observe` that returns the
/// transition. Announcing off the `hold` predicate instead would write one per
/// tick from each loop, which is the event-log flood this change exists to
/// prevent, one level up.
#[tokio::test]
async fn two_loops_across_one_edge_announce_the_hold_once() {
    let (_tmp, store, mut config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let _ = insert_task(&store, &project, 1, "waits for a slot").await;
    let (_task, build) = queued_build(&store, &project).await;
    store.set_mode(Mode::Play).await.unwrap();
    config.github_token = Some("token".into());
    config.builder_timeout = Duration::from_secs(10);

    let squatter_client: Client<TasksProtocol> =
        Client::connect(&config.vm_pool_socket).await.unwrap();
    let squatter = squatter_client
        .handle()
        .allocate("agent:v1", VmConfig::default())
        .await
        .expect("the only slot");

    // One shared record, exactly as `run()` builds it.
    let pool = test_pool_health();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let scouts = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config.clone(),
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        pool.clone(),
        shutdown_rx.clone(),
    ));
    let builds = tokio::spawn(run::build_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        pool.clone(),
        shutdown_rx,
    ));

    // Several probe intervals' worth of ticks from both loops.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let held = dispatcher_notes(&store).await;
    assert_eq!(
        held.len(),
        1,
        "two loops, one edge, one note — got {held:?}"
    );
    assert!(held[0].contains("no free slot"), "{}", held[0]);

    // And the same across the *other* edge.
    squatter_client
        .handle()
        .deallocate(&squatter)
        .await
        .unwrap();
    let s = store.clone();
    wait_until(Duration::from_secs(30), || {
        let s = s.clone();
        async move { dispatcher_notes(&s).await.len() >= 2 }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let both = dispatcher_notes(&store).await;
    assert_eq!(both.len(), 2, "the release is one edge too: {both:?}");
    assert!(both[1].contains("freed a slot"), "{}", both[1]);

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(60), scouts).await;
    let _ = tokio::time::timeout(Duration::from_secs(60), builds).await;

    // Both loops woke to the same freed slot and one of them lost the race —
    // the probe-to-allocate window #967 leaves open, and the one place #930's
    // waiver still applies to a live dispatch. It is *not* asserted here: this
    // harness hands the build lane a scout supervisor, so a build that takes
    // the slot fails as an ordinary verdict and the two outcomes would be
    // indistinguishable. The refusal's classification is pinned end to end in
    // `tests/scout.rs`, against a real `pool exhausted` over a real socket.
    let _ = store.get_build(&build.id).await;
}

/// The `Note`s the capacity hold wrote — its two edges and nothing else. Note
/// the filter is the transition wording rather than "vm-pool": #930's waiver
/// notes name vm-pool too, and counting those as edges would be counting the
/// wrong thing.
async fn dispatcher_notes(store: &Store) -> Vec<String> {
    store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::Note { message, .. }
                if message.contains("no free slot") || message.contains("freed a slot") =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect()
}

/// One approved spec, by the path a real one takes, with a build queued behind
/// it — the state the serial lane wakes up to.
async fn queued_build(store: &Store, project: &Project) -> (Task, Build) {
    let now = Utc::now();
    let task = insert_task(store, project, 90, "wants building").await;
    store
        .update_task_state(&task.id, TaskState::ReadyToBuild)
        .await
        .unwrap();
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
        .create_build(&[spec.id], "main", DecisionInput::human())
        .await
        .unwrap();
    (task, build)
}

#[tokio::test]
async fn poll_ingests_issues_once_and_tracks_closures() {
    let url = spawn_fake_github(vec![
        page(vec![issue(1, "first", "OPEN"), issue(2, "second", "OPEN")]),
        // Second pass: same issues, but #1 has been closed upstream.
        page(vec![
            issue(1, "first", "CLOSED"),
            issue(2, "second", "OPEN"),
        ]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    let project = insert_project(&store).await;

    let ingested = poll_once(&store, &github, &IntakeFilter::All, "main")
        .await
        .unwrap();
    assert_eq!(ingested, 2);
    let tasks = store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().all(|t| t.state == TaskState::Backlog));
    let ingest_events = store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.payload, EventPayload::TaskIngested { .. }))
        .count();
    assert_eq!(ingest_events, 2);

    // Re-poll: nothing new, and the closed issue's task is marked closed
    // without being re-ingested or losing its id.
    let ingested = poll_once(&store, &github, &IntakeFilter::All, "main")
        .await
        .unwrap();
    assert_eq!(ingested, 0);
    let after = store.list_tasks().await.unwrap();
    assert_eq!(after.len(), 2);
    let closed = after
        .iter()
        .find(|t| t.gh_issue_number == 1)
        .expect("task for issue 1");
    assert_eq!(closed.gh_state, GhState::Closed);
    assert_eq!(closed.state, TaskState::Backlog, "our state is ours to set");
    assert_eq!(
        closed.id,
        tasks
            .iter()
            .find(|t| t.gh_issue_number == 1)
            .unwrap()
            .id
            .clone()
    );
    assert_eq!(project.id, closed.project_id);
}

/// Every `task_gh_state_changed` on the log, oldest first.
async fn gh_state_changes(store: &Store) -> Vec<(TaskId, GhState)> {
    store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::TaskGhStateChanged { task_id, gh_state } => Some((task_id, gh_state)),
            _ => None,
        })
        .collect()
}

/// GitHub never says "issue 3 was closed" — it just stops returning it from the
/// open-issue query. A poll that sees a complete open set has to read that
/// absence as a closure, or the row stays `open` forever and clients render a
/// phantom task.
#[tokio::test]
async fn poll_closes_tasks_whose_issues_left_the_open_set() {
    let url = spawn_fake_github(vec![
        page(vec![
            issue(1, "first", "OPEN"),
            issue(2, "second", "OPEN"),
            issue(3, "third", "OPEN"),
        ]),
        // A bulk cleanup upstream: only #2 is still open.
        page(vec![issue(2, "second", "OPEN")]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    let project = insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        3
    );
    let before = store.list_tasks().await.unwrap();
    assert!(before.iter().all(|t| t.gh_state == GhState::Open));
    let id_of = |number: u64| {
        before
            .iter()
            .find(|t| t.gh_issue_number == number)
            .unwrap()
            .id
            .clone()
    };

    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        0,
        "nothing new to ingest"
    );

    let after = store.list_tasks().await.unwrap();
    assert_eq!(after.len(), 3, "rows are marked, never deleted");
    for (number, expected) in [
        (1, GhState::Closed),
        (2, GhState::Open),
        (3, GhState::Closed),
    ] {
        let task = after
            .iter()
            .find(|t| t.gh_issue_number == number)
            .unwrap_or_else(|| panic!("task for issue {number}"));
        assert_eq!(task.gh_state, expected, "issue {number}");
        assert_eq!(task.state, TaskState::Backlog, "our state is ours to set");
        assert_eq!(task.id, id_of(number), "same row, not a re-ingest");
    }
    assert_eq!(project.id, after[0].project_id);

    let mut changes = gh_state_changes(&store).await;
    changes.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut expected = vec![(id_of(1), GhState::Closed), (id_of(3), GhState::Closed)];
    expected.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    assert_eq!(changes, expected);

    // The canned server repeats its last page, so a third poll sees the same
    // open set: no further writes, no duplicate events.
    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        0
    );
    assert_eq!(gh_state_changes(&store).await.len(), 2);
}

/// A failed fetch is not an empty repository. Reconciling on a partial open set
/// would close every task in the project.
#[tokio::test]
async fn a_failed_fetch_reconciles_nothing() {
    let url = spawn_fake_github(vec![
        page(vec![issue(1, "first", "OPEN"), issue(2, "second", "OPEN")]),
        json!({"errors": [{"message": "Bad credentials"}]}),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;
    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        2
    );

    // The project is skipped, not failed: intake for other projects goes on.
    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        0
    );

    let tasks = store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(
        tasks.iter().all(|t| t.gh_state == GhState::Open),
        "an unreachable repo must not look like a closed one"
    );
    assert!(gh_state_changes(&store).await.is_empty());
}

/// Reopening rides the ordinary upsert path: the issue is back in the open set,
/// so the snapshot is refreshed with it.
#[tokio::test]
async fn a_reopened_issue_polls_back_to_open() {
    let url = spawn_fake_github(vec![
        page(vec![issue(1, "first", "OPEN")]),
        page(vec![]),
        page(vec![issue(1, "first", "OPEN")]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        1
    );
    let task_id = store.list_tasks().await.unwrap()[0].id.clone();

    poll_once(&store, &github, &IntakeFilter::All, "main")
        .await
        .unwrap();
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().gh_state,
        GhState::Closed
    );

    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        0,
        "a reopened issue is the same task, not a new one"
    );
    let reopened = store.get_task(&task_id).await.unwrap().unwrap();
    assert_eq!(reopened.gh_state, GhState::Open);
    assert_eq!(store.list_tasks().await.unwrap().len(), 1);
    assert_eq!(
        gh_state_changes(&store).await,
        vec![(task_id, GhState::Closed)],
        "only the poller-inferred closure needs an event of its own"
    );
}

// --- per-repo status (#903) ---

/// Archiving stops the *upsert* half of the poll and nothing else.
///
/// Closure is only ever learned from absence in the open set, so an archived
/// project that stopped being fetched would leave every task it already has
/// stuck at `gh_state = open` forever — and a Builder PR it already opened
/// would sit in `awaiting_merge` with nothing to make it loud. Exactly the
/// semantics an issue losing its `TASKS_INTAKE_LABEL` already has.
#[tokio::test]
async fn an_archived_project_stops_ingesting_but_keeps_reconciling() {
    let url = spawn_fake_github(vec![
        page(vec![issue(1, "first", "OPEN")]),
        // While archived: #1 is gone (closed upstream) and #2 is new.
        page(vec![issue(2, "second", "OPEN")]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    let project = insert_project(&store).await;
    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        1
    );
    let existing = store.list_tasks().await.unwrap()[0].id.clone();

    store
        .set_project_status(&project.id, ProjectStatus::Archived)
        .await
        .unwrap();
    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        0,
        "an archived repo gains no tasks"
    );
    let tasks = store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1, "#2 was never ingested");
    assert_eq!(
        store.get_task(&existing).await.unwrap().unwrap().gh_state,
        GhState::Closed,
        "but the work it already has still tracks upstream closure"
    );
}

/// Paused is the middle rung: issues still arrive, nothing is dispatched.
#[tokio::test]
async fn a_paused_project_still_ingests() {
    let url = spawn_fake_github(vec![page(vec![
        issue(1, "first", "OPEN"),
        issue(2, "second", "OPEN"),
    ])])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    let project = insert_project(&store).await;
    store
        .set_project_status(&project.id, ProjectStatus::Paused)
        .await
        .unwrap();

    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        2
    );
    assert!(
        store
            .list_tasks()
            .await
            .unwrap()
            .iter()
            .all(|t| t.state == TaskState::Backlog),
        "and they land in the backlog, which never dispatches anyway"
    );
}

// --- intake label filter (#761) ---

/// The label filter narrows intake: an issue without the label is never turned
/// into a task, whatever else it carries.
#[tokio::test]
async fn a_label_filter_ingests_only_labelled_issues() {
    let url = spawn_fake_github(vec![page(vec![
        labelled_issue(1, "wanted", "OPEN", &["tasks"]),
        labelled_issue(2, "bare", "OPEN", &[]),
        labelled_issue(3, "other labels", "OPEN", &["bug", "docs"]),
        labelled_issue(4, "wanted among others", "OPEN", &["bug", "TASKS"]),
    ])])
    .await;
    let github = GitHubClient::with_base_url("token", url);
    let intake = IntakeFilter::from_label(Some("tasks".into()));

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &intake, "main").await.unwrap(),
        2
    );
    let mut numbers: Vec<u64> = store
        .list_tasks()
        .await
        .unwrap()
        .iter()
        .map(|t| t.gh_issue_number)
        .collect();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        vec![1, 4],
        "case-insensitive, absent-label-skipped"
    );
}

/// The default: unchanged behaviour for every deployment that never sets
/// `TASKS_INTAKE_LABEL`.
#[tokio::test]
async fn an_unset_filter_ingests_everything() {
    let url = spawn_fake_github(vec![page(vec![
        labelled_issue(1, "wanted", "OPEN", &["tasks"]),
        labelled_issue(2, "bare", "OPEN", &[]),
    ])])
    .await;
    let github = GitHubClient::with_base_url("token", url);

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &IntakeFilter::All, "main")
            .await
            .unwrap(),
        2
    );
    assert_eq!(store.list_tasks().await.unwrap().len(), 2);
}

/// Labelling an issue after the fact is an ordinary first sighting — no special
/// path, no backfill.
#[tokio::test]
async fn an_issue_that_gains_the_label_is_ingested_on_the_next_poll() {
    let url = spawn_fake_github(vec![
        page(vec![labelled_issue(1, "not yet", "OPEN", &[])]),
        page(vec![labelled_issue(1, "now labelled", "OPEN", &["tasks"])]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);
    let intake = IntakeFilter::from_label(Some("tasks".into()));

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &intake, "main").await.unwrap(),
        0
    );
    assert!(store.list_tasks().await.unwrap().is_empty());

    assert_eq!(
        poll_once(&store, &github, &intake, "main").await.unwrap(),
        1
    );
    let tasks = store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "now labelled");
    assert_eq!(tasks[0].state, TaskState::Backlog);
}

/// Un-labelling is not a retraction mechanism. The task keeps its row, its
/// queue position and its state; it just stops having its snapshot refreshed.
///
/// The `gh_state` assertion here is the regression guard for the whole design:
/// it fails the moment the filter is moved ahead of `open_numbers`, because
/// then absence-reconciliation reads the skipped issue as closed.
#[tokio::test]
async fn a_task_whose_issue_loses_the_label_is_kept_and_left_alone() {
    let url = spawn_fake_github(vec![
        page(vec![labelled_issue(1, "labelled", "OPEN", &["tasks"])]),
        page(vec![labelled_issue(1, "renamed upstream", "OPEN", &[])]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);
    let intake = IntakeFilter::from_label(Some("tasks".into()));

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &intake, "main").await.unwrap(),
        1
    );
    let task_id = store.list_tasks().await.unwrap()[0].id.clone();
    // Picked up by a human before the label came off.
    store
        .update_task_state(&task_id, TaskState::Queued)
        .await
        .unwrap();
    store
        .set_queue_order(std::slice::from_ref(&task_id))
        .await
        .unwrap();

    assert_eq!(
        poll_once(&store, &github, &intake, "main").await.unwrap(),
        0,
        "nothing new to ingest"
    );

    let after = store.get_task(&task_id).await.unwrap().unwrap();
    assert_eq!(after.gh_state, GhState::Open, "the issue is still open");
    assert_eq!(after.state, TaskState::Queued, "our state is ours to set");
    assert_eq!(after.manual_rank, Some(1), "and its queue slot is kept");
    assert_eq!(after.dispatch_attempts, 0);
    assert_eq!(
        after.title, "labelled",
        "the snapshot simply stops being refreshed"
    );
    assert!(gh_state_changes(&store).await.is_empty());
}

/// The payoff of filtering after the fetch rather than in the query: a task the
/// filter now skips still tracks upstream closure, because reconciliation keeps
/// seeing the complete open set.
#[tokio::test]
async fn an_unlabelled_task_still_tracks_upstream_closure() {
    let url = spawn_fake_github(vec![
        page(vec![labelled_issue(1, "labelled", "OPEN", &["tasks"])]),
        // Label removed upstream, issue still open: skipped by intake.
        page(vec![labelled_issue(1, "labelled", "OPEN", &[])]),
        // And now closed: gone from the open set entirely.
        page(vec![]),
    ])
    .await;
    let github = GitHubClient::with_base_url("token", url);
    let intake = IntakeFilter::from_label(Some("tasks".into()));

    let store = Store::open_in_memory().await.unwrap();
    insert_project(&store).await;

    assert_eq!(
        poll_once(&store, &github, &intake, "main").await.unwrap(),
        1
    );
    let task_id = store.list_tasks().await.unwrap()[0].id.clone();

    poll_once(&store, &github, &intake, "main").await.unwrap();
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().gh_state,
        GhState::Open
    );

    poll_once(&store, &github, &intake, "main").await.unwrap();
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().gh_state,
        GhState::Closed,
        "absence from the open set is still a closure"
    );
    assert_eq!(
        gh_state_changes(&store).await,
        vec![(task_id, GhState::Closed)]
    );
}

/// The wiring, not just the predicate: `Config.intake` has to reach `poll_once`
/// through `poll_loop`.
#[tokio::test]
async fn poll_loop_honours_the_configured_intake_label() {
    let github_url = spawn_fake_github(vec![page(vec![
        labelled_issue(1, "wanted", "OPEN", &["tasks"]),
        labelled_issue(2, "bare", "OPEN", &[]),
    ])])
    .await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    insert_project(&store).await;
    store.set_mode(Mode::Play).await.unwrap();

    let mut config = test_config(Path::new("/nonexistent"), Path::new("/nonexistent"), 1);
    config.github_token = Some("token".into());
    config.github_api_url = Some(github_url);
    config.poll_interval = Duration::from_millis(50);
    config.intake = IntakeFilter::from_label(Some("tasks".into()));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::poll_loop(
        store.clone(),
        config,
        Arc::new(GitHubHealth::default()),
        shutdown_rx,
    ));

    let s = store.clone();
    wait_until(Duration::from_secs(10), || {
        let s = s.clone();
        async move { !s.list_tasks().await.unwrap().is_empty() }
    })
    .await;
    // Several more passes: asserting the count right after the first sighting
    // would pass even if the unlabelled issue were merely one tick behind.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tasks = store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1, "the unlabelled issue never arrives");
    assert_eq!(tasks[0].gh_issue_number, 1);

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("poll loop exits on shutdown")
        .unwrap();
}

#[tokio::test]
async fn poll_loop_skips_polling_while_stopped() {
    let github_url = spawn_fake_github(vec![page(vec![issue(1, "only", "OPEN")])]).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    insert_project(&store).await;
    store.set_mode(Mode::Stop).await.unwrap();

    let mut config = test_config(Path::new("/nonexistent"), Path::new("/nonexistent"), 1);
    config.github_token = Some("token".into());
    config.github_api_url = Some(github_url);
    config.poll_interval = Duration::from_millis(50);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::poll_loop(
        store.clone(),
        config,
        Arc::new(GitHubHealth::default()),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        store.list_tasks().await.unwrap().is_empty(),
        "Stop must halt intake"
    );

    store.set_mode(Mode::Play).await.unwrap();
    let s = store.clone();
    wait_until(Duration::from_secs(10), || {
        let s = s.clone();
        async move { !s.list_tasks().await.unwrap().is_empty() }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("poll loop exits on shutdown")
        .unwrap();
}

#[tokio::test]
async fn dispatch_loop_survives_a_missing_vm_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "wants a scout").await;
    store.set_mode(Mode::Play).await.unwrap();

    // Nothing is listening on this socket, and nothing ever will be.
    let config = test_config(&tmp.path().join("absent.sock"), tmp.path(), 1);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(store.list_sessions().await.unwrap().is_empty());
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::Queued,
        "an undispatchable task stays queued"
    );

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("dispatch loop exits on shutdown while waiting to reconnect")
        .unwrap();
}

// --- scout dispatch loop ---

/// A Config that talks to this test's vm-pool socket and clones from a local
/// directory of fixture repos instead of github.com.
fn test_config(vm_pool_socket: &Path, clone_root: &Path, max_concurrent: usize) -> Config {
    Config {
        data_dir: clone_root.to_path_buf(),
        port: 0,
        poll_interval: Duration::from_secs(3600),
        startup_mode: tasks::run::DEFAULT_STARTUP_MODE,
        scout_max_concurrent: max_concurrent,
        scout_image: "agent:v1".into(),
        scout_timeout: Duration::from_secs(300),
        vm_pool_socket: vm_pool_socket.to_path_buf(),
        github_token: None,
        github_api_url: None,
        intake: IntakeFilter::All,
        clone_url_base: format!("file://{}", clone_root.display()),
        scout_base_branch: "main".into(),
        vm_config: VmConfig::default(),
        builder_vm_config: VmConfig::default(),
        builder_image: "builder:v1".into(),
        builder_timeout: Duration::from_secs(300),
        github_rest_api_url: None,
        orchestrator_cmd: "true".into(),
        orchestrator_timeout: Duration::from_secs(60),
        orchestrator_workdir: None,
        orchestrator_target_dir: None,
        update_hold: true,
    }
}

/// Full dispatch harness: fixture repo at `<tmp>/repos/test/repo.git`, a
/// vm-pool service running the real supervisor against the stub agent, and a
/// store on disk. Returns (tmpdir, store, config).
async fn dispatch_harness(
    max_concurrent: usize,
) -> (
    tempfile::TempDir,
    Arc<Store>,
    Config,
    Arc<Service<SupervisorRuntime, TasksProtocol>>,
) {
    dispatch_harness_with_agent(max_concurrent, stub_agent_path().to_str().unwrap()).await
}

/// [`dispatch_harness`] with the scout's agent command spelled out. `true`
/// gives an agent that exits cleanly without writing `SPEC.md`, which the
/// supervisor reports as a scout failure — a task that can never be scouted.
async fn dispatch_harness_with_agent(
    max_concurrent: usize,
    agent_cmd: &str,
) -> (
    tempfile::TempDir,
    Arc<Store>,
    Config,
    Arc<Service<SupervisorRuntime, TasksProtocol>>,
) {
    dispatch_harness_with_agent_env(max_concurrent, agent_cmd, &[]).await
}

/// [`dispatch_harness_with_agent`], plus environment for the supervisor
/// *inside* the VM — `SCOUT_MAX_RESUMES` has no other seam from out here.
async fn dispatch_harness_with_agent_env(
    max_concurrent: usize,
    agent_cmd: &str,
    supervisor_env: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    Arc<Store>,
    Config,
    Arc<Service<SupervisorRuntime, TasksProtocol>>,
) {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let clone_root = tmp.path().join("repos");
    make_fixture_repo(&clone_root.join("test"), "repo.git").await;

    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper_with_env(
        tmp.path(),
        &supervisor_bin,
        agent_cmd,
        &workdir_root,
        supervisor_env,
    )
    .await;
    let (service, socket) = spawn_vm_pool(tmp.path(), &wrapper, max_concurrent.max(1)).await;

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let config = test_config(&socket, &clone_root, max_concurrent);
    (tmp, store, config, service)
}

async fn insert_task(store: &Store, project: &Project, number: u64, title: &str) -> Task {
    insert_task_with_gh_state(store, project, number, title, GhState::Open).await
}

async fn insert_task_with_gh_state(
    store: &Store,
    project: &Project,
    number: u64,
    title: &str,
    gh_state: GhState,
) -> Task {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: number,
        title: title.into(),
        body: format!("body of {title}"),
        labels: vec![],
        gh_state,
        state: TaskState::Queued,
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

/// Task ids in the order the dispatcher started sessions for them.
async fn dispatch_order(store: &Store) -> Vec<TaskId> {
    store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::SessionStarted { task_id, .. } => Some(task_id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn dispatch_loop_follows_queue_order_and_skips_closed_issues() {
    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;

    let one = insert_task(&store, &project, 1, "one").await;
    let two = insert_task(&store, &project, 2, "two").await;
    let three = insert_task(&store, &project, 3, "three").await;
    // Closed upstream: never worth a scout, whatever its rank.
    let closed = insert_task_with_gh_state(&store, &project, 4, "closed", GhState::Closed).await;
    // Backlog: ingested but never picked up — invisible to the dispatcher.
    let backlog = insert_task(&store, &project, 5, "backlog").await;
    store
        .update_task_state(&backlog.id, TaskState::Backlog)
        .await
        .unwrap();

    // Human-curated order, deliberately not insertion order.
    store
        .set_queue_order(&[
            closed.id.clone(),
            three.id.clone(),
            one.id.clone(),
            two.id.clone(),
        ])
        .await
        .unwrap();
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 3 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    // One scout at a time, in queue order.
    assert_eq!(
        dispatch_order(&store).await,
        vec![three.id.clone(), one.id.clone(), two.id.clone()]
    );

    for task in [&three, &one, &two] {
        let stored = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, TaskState::InReview, "task {}", task.id);
    }
    let specs = store.list_specs().await.unwrap();
    assert!(specs.iter().all(|s| s.content.contains("## Spec")));
    assert_eq!(store.list_spec_queue().await.unwrap().len(), 3);

    let stored_closed = store.get_task(&closed.id).await.unwrap().unwrap();
    assert_eq!(
        stored_closed.state,
        TaskState::Queued,
        "a closed issue must never be scouted"
    );
    let stored_backlog = store.get_task(&backlog.id).await.unwrap().unwrap();
    assert_eq!(
        stored_backlog.state,
        TaskState::Backlog,
        "backlog work is never dispatched — queue membership is explicit"
    );
}

#[tokio::test]
async fn pause_blocks_new_dispatches() {
    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let first = insert_task(&store, &project, 1, "first").await;
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 1 }
    })
    .await;

    // Pause, then queue more work: the loop must leave it alone. The two
    // writes are adjacent on purpose, and the ordering is the whole assertion.
    // The dispatcher re-reads the mode after it scans the queue and
    // immediately before it spawns, so a task committed after this pause
    // cannot be dispatched by any interleaving: whatever pass sees the task
    // also sees the pause.
    store.set_mode(Mode::Pause).await.unwrap();
    let second = insert_task(&store, &project, 2, "second").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        store.list_sessions().await.unwrap().len(),
        1,
        "Pause must not start new scouts"
    );
    assert_eq!(
        store.get_task(&second.id).await.unwrap().unwrap().state,
        TaskState::Queued
    );

    // Resuming picks the queued task straight up.
    store.set_mode(Mode::Play).await.unwrap();
    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 2 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    assert_eq!(
        dispatch_order(&store).await,
        vec![first.id.clone(), second.id.clone()]
    );
}

// --- crash consistency ---

/// Poll until `task` reaches `state`, then let the loop run a little longer so
/// any dispatch it was still going to start would show up in the assertions.
async fn wait_for_state(store: &Arc<Store>, task_id: &TaskId, state: TaskState) {
    let s = store.clone();
    let id = task_id.clone();
    wait_until(Duration::from_secs(120), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_task(&id).await.unwrap().unwrap().state == state }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
}

/// A task whose scout can never succeed gets three tries and is then rejected,
/// with the count on the row so a later process can't hand it three more.
///
/// This is also #884's negative case: an agent that ran to completion and
/// produced nothing usable *is* a verdict on the work, and still burns its
/// three. Without this the classification would be indistinguishable from
/// having switched the cap off.
#[tokio::test]
async fn an_agent_that_concluded_with_nothing_still_burns_its_three() {
    let (_tmp, store, config, _service) = dispatch_harness_with_agent(1, "true").await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "no spec, ever").await;
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));
    wait_for_state(&store, &task.id, TaskState::Rejected).await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Rejected);
    assert_eq!(stored.dispatch_attempts, 3, "strikes are persisted");
    assert_eq!(
        dispatch_order(&store).await,
        vec![task.id.clone(); 3],
        "exactly three dispatches, no more"
    );
    assert!(store.list_specs().await.unwrap().is_empty());
    assert!(
        store
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .all(|s| s.status == SessionStatus::ScoutFailed)
    );

    let payloads: Vec<_> = store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.payload)
        .collect();
    assert!(
        payloads.iter().any(|p| matches!(
            p,
            EventPayload::TaskStateChanged { task_id, to: TaskState::Rejected, .. }
                if *task_id == task.id
        )),
        "the rejection is on the event log"
    );
    assert!(
        payloads.iter().any(|p| matches!(
            p,
            EventPayload::Note { source, message }
                if source == "dispatcher" && message.contains("rejecting")
        )),
        "with a breadcrumb saying why"
    );
}

/// #884, the other half of the test above: a scout that dies of something
/// unrelated to the work is charged nothing, however often it happens.
///
/// The agent's API connection drops on every attempt (#845), so the task never
/// gets a verdict — and three of those used to reject it having learned
/// nothing. Here it stays queued past the cap, its attempt count untouched,
/// with the waiver on the event log so an unspent strike is not silently
/// indistinguishable from a cap that has been switched off.
#[tokio::test]
async fn an_infrastructure_death_never_rejects_the_task() {
    // Resuming off: the supervisor's own retry loop is #845's fix and is
    // tested there. What is under test here is the host's accounting, and the
    // rising backoff would otherwise be paid on every dispatch.
    let (_tmp, store, config, _service) = dispatch_harness_with_agent_env(
        1,
        api_death_agent_path().to_str().unwrap(),
        &[("SCOUT_MAX_RESUMES", "0")],
    )
    .await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "the network, not the task").await;
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    // Four dispatches: one more than the cap that would have rejected it.
    let s = store.clone();
    wait_until(Duration::from_secs(180), || {
        let s = s.clone();
        async move { dispatch_order(&s).await.len() >= 4 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(
        stored.dispatch_attempts, 0,
        "an infrastructure death is not the task's fault"
    );
    assert_ne!(
        stored.state,
        TaskState::Rejected,
        "the task is still work worth doing"
    );

    // Salvage still travels: a transport death is worth the next attempt's
    // while, which is the whole reason not to charge for it.
    let sessions = store.list_sessions().await.unwrap();
    assert!(
        sessions
            .iter()
            .all(|s| s.status == SessionStatus::ScoutStoppedEarly),
        "sessions: {sessions:?}"
    );

    let notes: Vec<String> = store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::Note { source, message } if source == "dispatcher" => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        notes
            .iter()
            .any(|m| m.contains("failed as transport") && m.contains("keeps its")),
        "the waiver has to be legible on the log: {notes:?}"
    );
    assert!(
        !notes.iter().any(|m| m.contains("rejecting")),
        "nothing was rejected: {notes:?}"
    );
}

/// Restart simulation: a task carrying two strikes from a previous process gets
/// the one attempt it has left, not a fresh three.
#[tokio::test]
async fn a_restart_resumes_the_persisted_attempt_count() {
    let (_tmp, store, config, _service) = dispatch_harness_with_agent(1, "true").await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "already on thin ice").await;
    for expected in 1..=2 {
        assert_eq!(
            store.record_dispatch_failure(&task.id).await.unwrap(),
            expected
        );
    }
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));
    wait_for_state(&store, &task.id, TaskState::Rejected).await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    assert_eq!(
        dispatch_order(&store).await,
        vec![task.id.clone()],
        "one attempt was left, so one dispatch"
    );
    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Rejected);
    assert_eq!(stored.dispatch_attempts, 3);
}

/// A server killed mid-scout leaves a `running` session and a `Scouting` task.
/// Startup reconciliation clears both, and the freed task is scouted normally.
#[tokio::test]
async fn startup_reconciles_orphaned_work_before_dispatch() {
    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "was mid-scout when we died").await;

    // Exactly what `tasks serve` leaves behind when it dies mid-dispatch.
    store
        .update_task_state(&task.id, TaskState::Scouting)
        .await
        .unwrap();
    let orphan = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: Some("vm-from-a-previous-life".into()),
        branch: String::new(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
        directions: None,
    };
    store.insert_session(&orphan).await.unwrap();
    store.set_mode(Mode::Play).await.unwrap();

    // Same call, same position in the sequence as `run()`: reconcile first,
    // then start the loops.
    run::reconcile_startup(&store).await.unwrap();

    let reconciled = store.get_session(&orphan.id).await.unwrap().unwrap();
    assert_eq!(reconciled.status, SessionStatus::ScoutFailed);
    assert!(reconciled.completed_at.is_some());
    assert_eq!(
        reconciled.exit_reason.as_deref(),
        Some("orphaned by server restart")
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::Queued,
        "the stranded task is back in the queue"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 1 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::InReview);
    assert_eq!(
        stored.dispatch_attempts, 0,
        "a crashed server is not the task's fault"
    );
    assert_eq!(dispatch_order(&store).await, vec![task.id.clone()]);
}

/// #762: a hung scout is ended by its deadline, counts as a dispatch failure
/// like any other, and — the point of the feature — frees its slot so the
/// queue keeps moving.
#[tokio::test]
async fn a_hung_scout_times_out_and_frees_its_slot() {
    // 10s against a 2s deadline; see the comment in tests/scout.rs on why not
    // 300s. One slot, so the second task can only run once the first lets go.
    let (_tmp, store, mut config, service) = dispatch_harness_with_agent(1, "sleep 10").await;
    config.scout_timeout = Duration::from_secs(2);
    let project = insert_project(&store).await;
    let hung = insert_task(&store, &project, 1, "hangs forever").await;
    let queued = insert_task(&store, &project, 2, "waiting behind it").await;
    store
        .set_queue_order(&[hung.id.clone(), queued.id.clone()])
        .await
        .unwrap();
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        test_update_watch(),
        test_pool_health(),
        shutdown_rx,
    ));

    // Three timeouts exhaust the attempt cap, exactly as three of anything else.
    wait_for_state(&store, &hung.id, TaskState::Rejected).await;
    // The slot is free: the task behind it gets dispatched.
    wait_until(Duration::from_secs(60), || {
        let store = store.clone();
        let id = queued.id.clone();
        async move { dispatch_order(&store).await.contains(&id) }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();

    let stored = store.get_task(&hung.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Rejected);
    assert_eq!(
        stored.dispatch_attempts, 3,
        "a timeout is a dispatch failure"
    );

    let sessions = store.list_sessions().await.unwrap();
    let timed_out: Vec<_> = sessions
        .iter()
        .filter(|s| s.task_id == hung.id)
        .inspect(|s| assert_eq!(s.status, SessionStatus::ScoutFailed))
        .filter(|s| {
            s.exit_reason
                .as_deref()
                .is_some_and(|r| r.contains("timed out"))
        })
        .collect();
    assert_eq!(timed_out.len(), 3, "three sessions ended on the deadline");

    let payloads: Vec<_> = store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.payload)
        .collect();
    // The task returns to New after each timeout. Asserted on the append-only
    // log rather than by polling the row: the dispatch loop re-picks the task
    // within a tick, so a row read can legitimately miss the window. Don't
    // "fix" this back into a get_task().state check.
    let back_to_new = payloads
        .iter()
        .filter(|p| {
            matches!(
                p,
                EventPayload::TaskStateChanged {
                    task_id,
                    from: TaskState::Scouting,
                    to: TaskState::Queued
                } if *task_id == hung.id
            )
        })
        .count();
    assert_eq!(back_to_new, 3, "each timeout requeues the task");
    assert!(
        payloads.iter().any(|p| matches!(
            p,
            EventPayload::Note { source, message }
                if source == "dispatcher" && message.contains("timed out")
        )),
        "the dispatcher leaves a breadcrumb naming the timeout"
    );

    // Cancellation is deallocation — no VM may outlive its dispatch, or the
    // pool leaks a slot per hang.
    wait_until(Duration::from_secs(60), || {
        let service = service.clone();
        async move { service.pool.list().await.is_empty() }
    })
    .await;
}

/// The update hold, at the same gate: a stale image observed in the record
/// stops new scouts exactly as a GitHub outage does — nothing dispatched,
/// nothing moved, no attempt charged — and observing the rebuilt image
/// releases it without a restart.
///
/// The release half is not optional here either, and it is also the
/// no-wedge rule in action: the watch holds on *observed* staleness only, so
/// recording the current identity (what the first run in a rebuilt image
/// does) must reopen the gate by itself.
#[tokio::test]
async fn an_update_hold_starts_no_scout_and_observing_the_rebuilt_image_releases_it() {
    use tasks_protocol::SupervisorBuild;

    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "waits for the upgrade").await;
    store.set_mode(Mode::Play).await.unwrap();

    // The watch boots first: only an observation made under this server can
    // hold — one from before its boot is stale data about images that may
    // have been rebuilt since.
    let updates = test_update_watch();

    // A run reports an image older than this binary: the upgrade is
    // half-applied, and the observation is fresh.
    store
        .record_image_build(
            "agent:v1",
            tasks_api::version::ImageRole::Scout,
            Some(&SupervisorBuild {
                version: "0.1.1".into(),
                commit: "0000000".into(),
            }),
            "sess_before",
        )
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
        Arc::new(GitHubHealth::default()),
        updates,
        test_pool_health(),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        store.list_sessions().await.unwrap().is_empty(),
        "a held dispatcher starts nothing"
    );
    let held = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(held.state, TaskState::Queued, "and moves nothing");
    assert_eq!(held.dispatch_attempts, 0, "holding charges nothing");

    // The rebuilt image is observed — the write the first run in it makes —
    // and dispatch resumes with no restart in between.
    store
        .record_image_build(
            "agent:v1",
            tasks_api::version::ImageRole::Scout,
            Some(&SupervisorBuild {
                version: tasks::version::VERSION.into(),
                commit: "0000000".into(),
            }),
            "sess_after",
        )
        .await
        .unwrap();
    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 1 }
    })
    .await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatch loop exits on shutdown")
        .unwrap();
    assert_eq!(
        store
            .get_task(&task.id)
            .await
            .unwrap()
            .unwrap()
            .dispatch_attempts,
        0
    );
}
