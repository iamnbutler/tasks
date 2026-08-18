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
    DecisionInput, GhState, Project, ProjectId, ProjectStatus, SessionId, SessionStatus,
    SpecQueueStatus, Task, TaskId, TaskState,
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
        status: ProjectStatus::Active,
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
        scout_directions: None,
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
        .get_session(
            stored_spec
                .session_id
                .as_ref()
                .expect("a scouted spec has a session"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.status, SessionStatus::ScoutSucceeded);
    assert!(session.branch.starts_with("scout/"));

    // The image identity, all the way through: the *real* supervisor binary
    // stamped itself, reported it on `Started`, and the host recorded it. This
    // and its Builder counterpart are the only places the whole chain is
    // actually checked — everywhere else `Option` plus `serde(default)` is by
    // design indistinguishable from "the field was never sent", so a break
    // would pass silently.
    let images = store.image_builds("0.1.0").await.unwrap();
    let observed = images
        .iter()
        .find(|i| i.image == "agent:v1")
        .expect("the scout image was observed");
    assert_eq!(observed.role, tasks_api::version::ImageRole::Scout);
    assert!(
        observed.version.is_some(),
        "the supervisor stated no build identity"
    );
    assert!(observed.commit.is_some());
    assert_eq!(observed.run_id.as_deref(), Some(session.id.as_str()));

    // Sanity: event log captured the transitions
    let events = store.all_events().await.unwrap();
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
            DecisionInput::human(),
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

/// #835 end to end: a scout that dies before concluding leaves salvage behind,
/// and that salvage never touches the review queue.
///
/// The two `assert!(...is_empty())` calls are the regression guard for the
/// whole design. The obvious fix for a lost run — report the partial spec — is
/// worse than the bug it fixes, because a half-explored spec entering the
/// queue looks finished. Keep them.
#[tokio::test]
async fn an_interrupted_scout_is_salvaged_without_producing_a_spec() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        common::interrupted_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) =
        insert_project_and_task(&store, "Runs out of road", "The issue body.").await;

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

    let err = scout
        .dispatch(task.clone(), &target)
        .await
        .expect_err("a run without a spec is not a success");
    assert!(
        matches!(err, tasks::scout::ScoutError::StoppedEarly { .. }),
        "stopping early is its own outcome, not a generic failure: {err:?}"
    );
    // An agent that ran to completion and produced no spec is a verdict, and
    // is charged for it. Waiving this would be switching the cap off.
    assert_eq!(
        err.failure_class(),
        tasks::protocol::FailureClass::Verdict,
        "an agent that concluded with nothing judged the work: {err:?}"
    );

    // Nothing reviewable exists. This is the invariant.
    assert!(
        store.list_specs().await.unwrap().is_empty(),
        "a half-explored run must not produce a spec"
    );
    assert!(
        store.list_spec_queue().await.unwrap().is_empty(),
        "and must not reach the review queue by any other route"
    );

    // A third terminal outcome, neither success nor failure.
    let sessions = store.list_sessions().await.unwrap();
    let session = sessions.last().expect("a session row");
    assert_eq!(session.status, SessionStatus::ScoutStoppedEarly);
    assert!(
        session
            .exit_reason
            .as_deref()
            .is_some_and(|r| r.contains("not a spec yet")),
        "exit_reason: {:?}",
        session.exit_reason
    );

    // The salvage itself, in the one place it lives.
    let notes = store
        .get_scout_notes(&session.id)
        .await
        .unwrap()
        .expect("notes were salvaged");
    assert!(notes.notes.contains("src/parse.rs"), "{}", notes.notes);
    assert!(notes.notes.contains("Nothing below is a spec"));
    assert!(notes.reason.is_some());
    assert!(notes.files_touched.contains(&"src/half.rs".to_string()));

    // The task is picked up still, and the attempt is on the record.
    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Queued);
}

/// The salvage's only consumer: the next attempt's prompt. The echo-prompt
/// agent copies its whole stdin into SPEC.md, so the second run's spec content
/// *is* the prompt the second scout was handed.
#[tokio::test]
async fn the_next_scout_is_handed_the_field_notes_as_unverified_leads() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store, "Two tries", "The issue body.").await;
    let scout_config = ScoutConfig {
        image: "agent:v1".into(),
        vm_config: VmConfig::default(),
        timeout: Duration::from_secs(300),
    };

    // First attempt: interrupted, so it leaves notes.
    {
        let wrapper = write_supervisor_wrapper(
            tmp.path(),
            &supervisor_bin,
            common::interrupted_agent_path().to_str().unwrap(),
            &workdir_root,
        )
        .await;
        let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
        let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();
        let scout = Scout::new(store.clone(), client.handle(), scout_config.clone());
        scout
            .dispatch(task.clone(), &target)
            .await
            .expect_err("interrupted");
    }

    // Second attempt: the prompt should carry the first run's notes.
    let requeued = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(requeued.state, TaskState::Queued);

    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        common::echo_prompt_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();
    let scout = Scout::new(store.clone(), client.handle(), scout_config);
    let spec = scout.dispatch(requeued, &target).await.expect("second run");

    assert!(
        spec.content
            .contains("## Field notes from an interrupted attempt"),
        "the second prompt is missing the salvage:\n{}",
        spec.content
    );
    assert!(
        spec.content.contains("The parser lives in `src/parse.rs`"),
        "the notes themselves should be quoted"
    );
    assert!(
        spec.content.contains("Nothing below has been verified."),
        "salvage must arrive labelled as unverified"
    );

    // And once a scout concludes, the stale leads stop being handed out.
    assert!(
        store.salvage_for_task(&task.id).await.unwrap().is_none(),
        "a spec supersedes the notes that led to it"
    );
}

/// The salvage precondition, observed rather than raced.
///
/// A `scout_notes` row exists only once the checkpoint sink `Scout::follow`
/// spawns has been handed an event, and the drain hands it that event in the
/// same match arm that sets its own `state.checkpoint` — so the row is proof
/// that the dispatcher is holding a checkpoint, which is exactly the
/// precondition a salvaged timeout is about. `cancel.rs`'s
/// `a_cancelled_scout_keeps_its_salvage_and_stamps_the_reason` waits on this
/// same row before it cancels; this is that idiom with the deadline
/// substituted for the cancel.
///
/// Written out rather than routed through `common::wait_until` for two
/// reasons: the session id has to come back, and the failure has to name the
/// precondition that never arrived rather than say `condition not met`.
async fn await_streamed_checkpoint(store: &Store, within: Duration) -> SessionId {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(session) = store.list_sessions().await.unwrap().last()
            && store.get_scout_notes(&session.id).await.unwrap().is_some()
        {
            return session.id.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no scout_notes row within {within:?}: the dispatcher never streamed a checkpoint, \
             so a deadline fired now would be testing the wrong thing"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Retire a running budget from the harness instead of waiting it out.
///
/// [`tasks::deadline::Deadline`] anchors on a `tokio::time::Instant` —
/// deliberately, so its poll loop still advances under a paused clock (see the
/// doc comment on `Deadline::awake_from`) — so pausing, jumping past the
/// budget and resuming is the one seam that expires a dispatch on demand
/// without a test-only knob in production code.
///
/// The pause spans a single `advance` and nothing real: a paused clock
/// auto-advances whenever the runtime parks, so holding it across store or
/// socket I/O would freeze nothing and would jump straight to the next timer.
/// The jump itself has to stay modest for the same reason `brief.rs` opens its
/// store before pausing — past 30s it can trip an in-flight sqlx pool acquire,
/// past 60s it wakes the test pool's health check, and past 300s the pool
/// reaps the VM out from under the run.
async fn expire_the_budget(budget: Duration) {
    tokio::time::pause();
    tokio::time::advance(budget + Duration::from_secs(1)).await;
    tokio::time::resume();
}

/// The headline case: the deadline. The VM is destroyed where it stands, so
/// nothing on its disk is recoverable and the supervisor never gets to report
/// anything — the last checkpoint the dispatcher already holds is the entire
/// salvage. That state lives outside the drain future precisely because
/// `tokio::time::timeout` drops it here.
///
/// The error keeps its shape: a salvaged timeout is still `Timeout`, and
/// `exit_reason` still says "timed out". CLAUDE.md and two other tests pin it.
///
/// What this proves is a deadline firing against a run that is *already
/// holding* a checkpoint. It does not exercise a deadline arriving naturally —
/// `a_scout_that_never_reports_back_times_out` and
/// `a_hung_scout_times_out_and_frees_its_slot` are for that. The budget here is
/// never waited out: the harness watches the checkpoint into the store and then
/// fires the deadline itself. The ordering is the whole guarantee. Because
/// `CHECKPOINT_WAIT` is strictly under `BUDGET`, the run's own deadline can
/// never fire while the harness is still watching, so a machine too slow to
/// stream a checkpoint fails on a named precondition rather than on a verdict
/// about salvage (#958 — a 3s budget against ~1-2s of checkpoint latency,
/// which passed alone and failed with seven siblings on the machine).
#[tokio::test]
async fn a_timed_out_scout_keeps_the_checkpoint_it_had_already_streamed() {
    /// Never waited out — see above. Capped at 20s because
    /// [`expire_the_budget`] jumps `BUDGET + 1s` and that jump has ceilings.
    const BUDGET: Duration = Duration::from_secs(20);
    /// Strictly under [`BUDGET`], which is what keeps the two failure modes
    /// from swapping places.
    const CHECKPOINT_WAIT: Duration = Duration::from_secs(10);

    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    // The wrapper sets a 1s checkpoint interval, and the supervisor's watcher
    // sleeps first, so the first checkpoint cannot land before ~1s. The agent's
    // own `sleep 10` outlives the run — see the note in
    // `a_scout_that_never_reports_back_times_out`.
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        common::notes_then_hangs_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store, "Hangs mid-thought", "body").await;

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            timeout: BUDGET,
        },
    );
    let target = ScoutTarget {
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };

    let dispatched = task.clone();
    let dispatch = tokio::spawn(async move { scout.dispatch(dispatched, &target).await });

    let streamed = await_streamed_checkpoint(&store, CHECKPOINT_WAIT).await;
    expire_the_budget(BUDGET).await;

    let err = dispatch.await.unwrap().expect_err("should time out");
    assert!(
        matches!(err, tasks::scout::ScoutError::Timeout { secs } if secs == BUDGET.as_secs()),
        "a salvaged timeout is still a timeout: {err:?}"
    );

    let sessions = store.list_sessions().await.unwrap();
    let session = sessions.last().expect("a session row");
    assert_eq!(
        session.id, streamed,
        "the session that timed out is the one whose checkpoint we watched land"
    );
    assert_eq!(session.status, SessionStatus::ScoutStoppedEarly);
    assert!(
        session
            .exit_reason
            .as_deref()
            .is_some_and(|r| r.contains("timed out")),
        "the timeout must not be rebranded: {:?}",
        session.exit_reason
    );

    let notes = store
        .get_scout_notes(&session.id)
        .await
        .unwrap()
        .expect("the streamed checkpoint outlived the VM");
    assert!(
        notes.notes.contains("this is all I have"),
        "notes: {}",
        notes.notes
    );

    // Everything the plain timeout path already guaranteed still holds.
    assert!(store.list_specs().await.unwrap().is_empty());
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::Queued
    );
    assert!(
        service.pool.list().await.is_empty(),
        "cancellation is deallocation; the slot must come back"
    );
}
