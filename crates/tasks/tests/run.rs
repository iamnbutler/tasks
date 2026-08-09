//! End-to-end tests for the server loops in [`tasks::run`].
//!
//! The poll tests run against a local axum server returning canned GraphQL
//! payloads; the dispatch tests run against a real vm-pool service driving the
//! real scout-supervisor binary with a stub agent. No mocks.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::Json as AxumJson;
use axum::routing::post;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::watch;
use vm_pool_manager::SupervisorRuntime;
use vm_pool_protocol::VmConfig;
use vm_pool_service::Service;

use tasks_protocol::TasksProtocol;

use tasks::events::EventPayload;
use tasks::github::GitHubClient;
use tasks::models::{GhState, Mode, Project, ProjectId, Task, TaskId, TaskState};
use tasks::run::{self, Config};
use tasks::store::Store;

mod common;
use common::{
    cargo_build, make_fixture_repo, spawn_vm_pool, stub_agent_path, wait_until,
    write_supervisor_wrapper,
};

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
    json!({
        "number": number,
        "title": title,
        "body": format!("body of {number}"),
        "state": state,
        "updatedAt": "2026-08-09T00:00:00Z",
        "labels": { "nodes": [] },
    })
}

async fn insert_project(store: &Store) -> Project {
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();
    project
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

    let ingested = run::poll_once(&store, &github).await.unwrap();
    assert_eq!(ingested, 2);
    let tasks = store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().all(|t| t.state == TaskState::New));
    let ingest_events = store
        .events_since(0)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.payload, EventPayload::TaskIngested { .. }))
        .count();
    assert_eq!(ingest_events, 2);

    // Re-poll: nothing new, and the closed issue's task is marked closed
    // without being re-ingested or losing its id.
    let ingested = run::poll_once(&store, &github).await.unwrap();
    assert_eq!(ingested, 0);
    let after = store.list_tasks().await.unwrap();
    assert_eq!(after.len(), 2);
    let closed = after
        .iter()
        .find(|t| t.gh_issue_number == 1)
        .expect("task for issue 1");
    assert_eq!(closed.gh_state, GhState::Closed);
    assert_eq!(closed.state, TaskState::New, "our state is ours to set");
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
    let handle = tokio::spawn(run::poll_loop(store.clone(), config, shutdown_rx));

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
    let handle = tokio::spawn(run::dispatch_loop(store.clone(), config, shutdown_rx));

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(store.list_sessions().await.unwrap().is_empty());
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::New,
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
        scout_max_concurrent: max_concurrent,
        scout_image: "agent:v1".into(),
        vm_pool_socket: vm_pool_socket.to_path_buf(),
        github_token: None,
        github_api_url: None,
        clone_url_base: format!("file://{}", clone_root.display()),
        vm_config: VmConfig::default(),
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
    let supervisor_bin = cargo_build("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let clone_root = tmp.path().join("repos");
    make_fixture_repo(&clone_root.join("test"), "repo.git").await;

    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        stub_agent_path().to_str().unwrap(),
        &workdir_root,
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
        state: TaskState::New,
        priority: 0,
        manual_rank: None,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    task
}

/// Task ids in the order the dispatcher started sessions for them.
async fn dispatch_order(store: &Store) -> Vec<TaskId> {
    store
        .events_since(0)
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
    let handle = tokio::spawn(run::dispatch_loop(store.clone(), config, shutdown_rx));

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
        assert_eq!(stored.state, TaskState::SpecReady, "task {}", task.id);
    }
    let specs = store.list_specs().await.unwrap();
    assert!(specs.iter().all(|s| s.content.contains("## Spec")));
    assert_eq!(store.list_spec_queue().await.unwrap().len(), 3);

    let stored_closed = store.get_task(&closed.id).await.unwrap().unwrap();
    assert_eq!(
        stored_closed.state,
        TaskState::New,
        "a closed issue must never be scouted"
    );
}

#[tokio::test]
async fn pause_blocks_new_dispatches() {
    let (_tmp, store, config, _service) = dispatch_harness(1).await;
    let project = insert_project(&store).await;
    let first = insert_task(&store, &project, 1, "first").await;
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(store.clone(), config, shutdown_rx));

    let s = store.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        async move { s.list_specs().await.unwrap().len() == 1 }
    })
    .await;

    // Pause, then queue more work: the loop must leave it alone.
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
        TaskState::New
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
