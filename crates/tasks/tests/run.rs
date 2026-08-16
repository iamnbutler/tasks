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
use tasks::github::{GitHubClient, IntakeFilter};
use tasks::models::{
    GhState, Mode, Project, ProjectId, Session, SessionId, SessionStatus, Task, TaskId, TaskState,
};
use tasks::run::{self, Config, InFlight};
use tasks::store::Store;

mod common;
use common::{
    make_fixture_repo, spawn_vm_pool, stub_agent_path, wait_until, workspace_bin,
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

    let ingested = run::poll_once(&store, &github, &IntakeFilter::All)
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
    let ingested = run::poll_once(&store, &github, &IntakeFilter::All)
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
        run::poll_once(&store, &github, &IntakeFilter::All)
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
        run::poll_once(&store, &github, &IntakeFilter::All)
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
        run::poll_once(&store, &github, &IntakeFilter::All)
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
        run::poll_once(&store, &github, &IntakeFilter::All)
            .await
            .unwrap(),
        2
    );

    // The project is skipped, not failed: intake for other projects goes on.
    assert_eq!(
        run::poll_once(&store, &github, &IntakeFilter::All)
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
        run::poll_once(&store, &github, &IntakeFilter::All)
            .await
            .unwrap(),
        1
    );
    let task_id = store.list_tasks().await.unwrap()[0].id.clone();

    run::poll_once(&store, &github, &IntakeFilter::All)
        .await
        .unwrap();
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().gh_state,
        GhState::Closed
    );

    assert_eq!(
        run::poll_once(&store, &github, &IntakeFilter::All)
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

    assert_eq!(run::poll_once(&store, &github, &intake).await.unwrap(), 2);
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
        run::poll_once(&store, &github, &IntakeFilter::All)
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

    assert_eq!(run::poll_once(&store, &github, &intake).await.unwrap(), 0);
    assert!(store.list_tasks().await.unwrap().is_empty());

    assert_eq!(run::poll_once(&store, &github, &intake).await.unwrap(), 1);
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

    assert_eq!(run::poll_once(&store, &github, &intake).await.unwrap(), 1);
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
        run::poll_once(&store, &github, &intake).await.unwrap(),
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

    assert_eq!(run::poll_once(&store, &github, &intake).await.unwrap(), 1);
    let task_id = store.list_tasks().await.unwrap()[0].id.clone();

    run::poll_once(&store, &github, &intake).await.unwrap();
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().gh_state,
        GhState::Open
    );

    run::poll_once(&store, &github, &intake).await.unwrap();
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
    let handle = tokio::spawn(run::poll_loop(store.clone(), config, shutdown_rx));

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
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
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
        briefing_cmd: "true".into(),
        briefing_ttl: Duration::from_secs(900),
        briefing_timeout: Duration::from_secs(60),
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
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let clone_root = tmp.path().join("repos");
    make_fixture_repo(&clone_root.join("test"), "repo.git").await;

    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, agent_cmd, &workdir_root).await;
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
        shutdown_rx,
    ));

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
#[tokio::test]
async fn three_failed_dispatches_reject_the_task() {
    let (_tmp, store, config, _service) = dispatch_harness_with_agent(1, "true").await;
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, 1, "no spec, ever").await;
    store.set_mode(Mode::Play).await.unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(run::dispatch_loop(
        store.clone(),
        config,
        InFlight::default(),
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
