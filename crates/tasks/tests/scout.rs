//! End-to-end scout dispatch integration test.
//!
//! Spins up:
//! - A real vm-pool-service backed by SupervisorRuntime pointing at the real
//!   scout-supervisor binary (compiled via cargo).
//! - A real vm-pool-client connected over a Unix socket.
//! - A real SQLite store on disk.
//! - A real local git repo fixture + stub-agent shell script.
//!
//! Then calls `Scout::dispatch(task, target)` and asserts the Spec + queue entry
//! land in the store with the expected state transitions. No mocks.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use vm_pool_protocol::VmConfig;

use tasks::models::{
    GhState, Project, ProjectId, SessionStatus, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::scout::{Scout, ScoutConfig, ScoutTarget};
use tasks::store::Store;
use tasks_protocol::TasksProtocol;
use vm_pool_client::Client;

mod common;
use common::{
    make_fixture_repo, spawn_vm_pool, stub_agent_path, workspace_bin, write_supervisor_wrapper,
};

async fn insert_project_and_task(store: &Store, title: &str, body: &str) -> (Project, Task) {
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();

    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 1,
        title: title.into(),
        body: body.into(),
        labels: vec!["test".into()],
        gh_state: GhState::Open,
        state: TaskState::Queued,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    (project, task)
}

#[tokio::test]
async fn scout_dispatch_end_to_end_produces_spec() {
    // 1. Locate binaries
    let supervisor_bin = workspace_bin("scout-supervisor").await;

    // 2. Set up tmpdir, fixture repo, wrapper
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        stub_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;

    // 3. Start vm-pool service + client
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    // 4. Set up store with a task
    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store, "Stub task", "Do the stub thing").await;

    // 5. Dispatch
    let scout_config = ScoutConfig {
        image: "agent:v1".into(),
        vm_config: VmConfig::default(),
        timeout: Duration::from_secs(300),
    };
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };
    let scout = Scout::new(store.clone(), client.handle(), scout_config);
    let spec = scout
        .dispatch(task.clone(), &target)
        .await
        .expect("dispatch");

    // 6. Assertions
    assert!(
        spec.content.contains("## Spec"),
        "spec content: {}",
        spec.content
    );
    assert!(spec.files_touched.iter().any(|f| f == "src/stub.rs"));

    let stored_task = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored_task.state, TaskState::InReview);

    let stored_spec = store.get_spec(&spec.id).await.unwrap().unwrap();
    assert_eq!(stored_spec.task_id, task.id);

    let queue_entry = store
        .get_spec_queue_entry(&spec.id)
        .await
        .unwrap()
        .expect("queue entry");
    assert_eq!(queue_entry.status, SpecQueueStatus::PendingReview);

    let session = store
        .get_session(&stored_spec.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, SessionStatus::ScoutSucceeded);
    assert!(session.branch.starts_with("scout/"));

    // Sanity: event log captured the transitions
    let events = store.events_since(0).await.unwrap();
    let state_changes: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.payload {
            tasks::events::EventPayload::TaskStateChanged { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    assert!(state_changes.contains(&(TaskState::Queued, TaskState::Scouting)));
    assert!(state_changes.contains(&(TaskState::Scouting, TaskState::InReview)));
}

#[tokio::test]
async fn two_scouts_dispatch_concurrently() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        stub_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task_a) = insert_project_and_task(&store, "Task A", "body a").await;
    let task_b = Task {
        id: TaskId::new(),
        gh_issue_number: 2,
        title: "Task B".into(),
        body: "body b".into(),
        ..task_a.clone()
    };
    store.insert_task(&task_b).await.unwrap();

    let scout_config = ScoutConfig {
        image: "agent:v1".into(),
        vm_config: VmConfig::default(),
        timeout: Duration::from_secs(300),
    };
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };
    // One dispatcher, one vm-pool connection, two simultaneous dispatches
    // (pool is sized max_vms: 2). Each dispatch filters its own VM's events.
    let scout = Scout::new(store.clone(), client.handle(), scout_config);
    let (a, b) = tokio::join!(
        scout.dispatch(task_a.clone(), &target),
        scout.dispatch(task_b.clone(), &target)
    );
    let spec_a = a.expect("dispatch a");
    let spec_b = b.expect("dispatch b");

    assert_eq!(spec_a.task_id, task_a.id);
    assert_eq!(spec_b.task_id, task_b.id);
    assert!(spec_a.content.contains("## Spec"));
    assert!(spec_b.content.contains("## Spec"));
    for t in [&task_a, &task_b] {
        let stored = store.get_task(&t.id).await.unwrap().unwrap();
        assert_eq!(stored.state, TaskState::InReview, "task {}", t.id);
    }
}

#[tokio::test]
async fn scout_dispatch_failure_resets_task_to_new() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    // Agent = `true` → exits cleanly but writes no SPEC.md → ScoutEvent::Failed
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, "true", &workdir_root).await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) =
        insert_project_and_task(&store, "Will fail", "No SPEC.md produced").await;

    let scout_config = ScoutConfig {
        image: "agent:v1".into(),
        vm_config: VmConfig::default(),
        timeout: Duration::from_secs(300),
    };
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };
    let scout = Scout::new(store.clone(), client.handle(), scout_config);
    let result = scout.dispatch(task.clone(), &target).await;
    assert!(
        result.is_err(),
        "expected dispatch to error, got {result:?}"
    );

    let stored_task = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(
        stored_task.state,
        TaskState::Queued,
        "failed scout should return the task to Queued for retry"
    );
}

/// #760: a task re-dispatched after `needs_revision` must receive the
/// reviewer's feedback and the spec it referred to.
///
/// The echo-prompt agent copies its whole stdin into SPEC.md, so the second
/// run's spec content *is* the prompt the scout was given.
#[tokio::test]
async fn re_scout_after_needs_revision_receives_the_review() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        common::echo_prompt_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store, "Needs work", "The issue body.").await;

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(300),
        },
    );
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };

    // First pass: a fresh task, so no "Previous attempt" anywhere.
    let first = scout.dispatch(task.clone(), &target).await.expect("first");
    assert!(
        !first.content.contains("## Previous attempt"),
        "a fresh task's prompt must be unchanged"
    );

    // Review it back.
    const FEEDBACK: &str = "Section 3 is underspecified — name the files.";
    store
        .review_spec(
            &first.id,
            SpecQueueStatus::NeedsRevision,
            Some(FEEDBACK.to_string()),
        )
        .await
        .unwrap();

    // Re-dispatch: the task is back in `New`.
    let requeued = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(requeued.state, TaskState::Queued);
    let second = scout.dispatch(requeued, &target).await.expect("second");

    assert!(
        second.content.contains(FEEDBACK),
        "re-scout prompt is missing the reviewer's feedback:\n{}",
        second.content
    );
    assert!(
        second.content.contains("## Previous attempt"),
        "re-scout prompt is missing the Previous attempt section"
    );
    assert!(
        second.content.contains("needs_revision"),
        "re-scout prompt should name the verdict"
    );
    // The prior spec travels with the feedback — feedback about "section 3" is
    // meaningless without section 3.
    assert!(
        second.content.contains("### Received prompt"),
        "the prior spec's own text should be quoted into the new prompt"
    );
}

/// #762: a scout whose agent never reports back is ended by the deadline
/// rather than holding its slot forever.
#[tokio::test]
async fn a_scout_that_never_reports_back_times_out() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    // A 10s sleep against a 2s deadline. Deliberately not 300s: deallocating a
    // supervisor *process* does not kill the agent under it (SupervisorRuntime
    // ::stop is a no-op and the supervisor is blocked on child.wait()), so a
    // long-sleeping stub keeps the test binary's inherited stderr open and makes
    // piped `cargo test` output look hung for the sleep's duration. 5x the
    // deadline is margin enough — keep the ratio if you retune.
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, "sleep 10", &workdir_root).await;
    let (service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store, "Hangs", "never finishes").await;

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(2),
        },
    );
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };

    let err = scout
        .dispatch(task.clone(), &target)
        .await
        .expect_err("should time out");
    assert!(
        matches!(err, tasks::scout::ScoutError::Timeout { secs: 2 }),
        "unexpected error: {err:?}"
    );

    // Session failed with a timeout reason, task returned for retry.
    let sessions = store.list_sessions().await.unwrap();
    let session = sessions.last().expect("a session row");
    assert_eq!(session.status, SessionStatus::ScoutFailed);
    assert!(
        session
            .exit_reason
            .as_deref()
            .is_some_and(|r| r.contains("timed out")),
        "exit_reason: {:?}",
        session.exit_reason
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::Queued
    );

    // Cancellation is deallocation: the VM must be gone, or the slot leaks.
    assert!(
        service.pool.list().await.is_empty(),
        "timed-out scout left its VM allocated"
    );
}
