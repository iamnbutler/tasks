//! Stopping a scout or a build that is already in flight (#876).
//!
//! The agents here are *gated* on a file the tests never create, so every run
//! is provably still going when the cancel arrives: a test that could pass by
//! the run having quietly finished first would prove nothing about cancelling.
//!
//! What each test asserts is deliberately not "the VM went away". Deallocating
//! is the easy half and was always one call away; the bug was that the
//! dispatcher stayed parked on a stream that would never speak again, leaving
//! the row `running`, the serial build lane occupied, and nothing saying the
//! cancel took. So these assert the *row concluded* and the *work came back*.
//!
//! **Do not run these through a pipe.** A cancelled run leaves an orphaned
//! supervisor holding the test binary's stdout, so `cargo test … | tail` never
//! sees EOF and looks hung for the full timeout — the same LEAK shape as the
//! scout timeout tests. `.config/nextest.toml` already passes those; redirect
//! to a file if you are using plain cargo.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use vm_pool_protocol::{VmConfig, VmId};

use tasks::builder::{Builder, BuilderConfig, BuilderError};
use tasks::events::EventPayload;
use tasks::github::GitHubClient;
use tasks::models::{
    Actor, BuildStatus, Capability, CharterLevel, Complexity, DecisionAction, DecisionInput,
    GhState, Project, ProjectId, ProjectStatus, RunKind, Session, SessionId, SessionStatus, Spec,
    SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::scout::{Scout, ScoutConfig, ScoutError, ScoutTarget};
use tasks::store::{Store, Strike};
use tasks_protocol::TasksProtocol;
use vm_pool_client::Client;

mod common;
use common::{
    gated_agent_path, gated_builder_agent_path, gated_notes_agent_path, make_fixture_repo,
    spawn_vm_pool, wait_until, workspace_bin, write_builder_supervisor_wrapper,
    write_supervisor_wrapper,
};

/// A server over `store`, plus the HTTP client the tests cancel through.
struct Api {
    base: String,
    http: reqwest::Client,
    store: Arc<Store>,
}

impl Api {
    async fn spawn(store: Arc<Store>) -> Self {
        let app = tasks::server::router(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            http: reqwest::Client::new(),
            store,
        }
    }

    async fn cancel(&self, path: &str, body: Value) -> (u16, Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap())
    }

    async fn cancel_as_orchestrator(&self, path: &str, body: Value) -> (u16, Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .header(
                "X-Tasks-Actor",
                format!("orchestrator {}", self.store.actor_token().expose()),
            )
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap())
    }
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

async fn insert_task(store: &Store, project: &Project, state: TaskState) -> Task {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 876,
        title: "gets stopped".into(),
        body: "the issue body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state,
        priority: 0,
        manual_rank: Some(1),
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
        scout_directions: None,
    };
    store.insert_task(&task).await.unwrap();
    task
}

/// A `ready_to_build` task with its session, spec, and approved queue entry.
async fn seed_approved(store: &Store, project: &Project) -> (Task, Spec) {
    let task = insert_task(store, project, TaskState::ReadyToBuild).await;
    let now = Utc::now();
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
        content: "## Spec: wants building\n\nAdd a function.".into(),
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
    (task, spec)
}

async fn wait_for_file(path: &Path) {
    let path = path.to_path_buf();
    wait_until(Duration::from_secs(120), || {
        let path = path.clone();
        async move { path.exists() }
    })
    .await;
}

/// The headline case: a scout somebody stopped concludes as `cancelled`, its
/// task comes back, and none of it is charged to the work.
#[tokio::test]
async fn a_cancelled_scout_concludes_and_returns_its_task_to_the_backlog() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    // The gate is never created. The run cannot end on its own.
    let gate = tmp.path().join("scout-gate");
    let agent_cmd = format!("{} {}", gated_agent_path().display(), gate.display());
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, &agent_cmd, &workdir_root).await;
    let (pool, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, TaskState::Queued).await;
    let api = Api::spawn(store.clone()).await;

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            // Far beyond the test's own patience: if this passes by timing out
            // it is not passing.
            timeout: Duration::from_secs(600),
            leases: None,
        },
    );
    let target = ScoutTarget {
        source: tasks::broker::CloneSource::Direct(format!("file://{}", repo.display())),
        base_branch: "main".into(),
    };
    let dispatched = task.clone();
    let dispatch = tokio::spawn(async move { scout.dispatch(dispatched, &target).await });

    wait_for_file(&gate.with_file_name("scout-gate.started")).await;
    let session = store.list_sessions().await.unwrap().pop().expect("session");
    assert_eq!(session.status, SessionStatus::Running);
    let vm_id = VmId::new(session.vm_id.clone().expect("the session recorded a VM"));

    let (status, ack) = api
        .cancel(
            &format!("/sessions/{}/cancel", session.id),
            json!({ "rationale": "the issue turned out to be a duplicate" }),
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["run_kind"], "session");
    assert_eq!(ack["concluded"], true, "{ack}");
    assert_eq!(ack["status"], "cancelled", "{ack}");

    // The dispatch returns the cancel as its own outcome — not a failure, and
    // not a timeout.
    let outcome = dispatch.await.unwrap();
    match outcome {
        Err(ScoutError::Cancelled(request)) => {
            assert_eq!(request.actor, Actor::Human);
            assert_eq!(
                request.rationale.as_deref(),
                Some("the issue turned out to be a duplicate")
            );
        }
        other => panic!("expected a cancel, got {other:?}"),
    }

    // The row concluded, and says who stopped it and why. This is the
    // assertion the issue is about: killing the container by hand left it
    // `running` forever.
    let concluded = store.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, SessionStatus::Cancelled);
    assert!(concluded.completed_at.is_some());
    assert_eq!(
        concluded.exit_reason.as_deref(),
        Some("cancelled by human: the issue turned out to be a duplicate")
    );

    // The work came back — to the *backlog*, not the queue, or the dispatch
    // loop would start a replacement scout within the tick.
    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Backlog);
    assert_eq!(stored.manual_rank, None, "a cancel leaves the queue");
    assert_eq!(
        stored.dispatch_attempts, 0,
        "a cancel is not a failed attempt"
    );

    // Nothing reviewable was invented on the way out.
    assert!(store.list_specs().await.unwrap().is_empty());
    assert!(store.list_spec_queue().await.unwrap().is_empty());

    // The announcement is on the record, with the ledger row behind it.
    let events = store.all_events().await.unwrap();
    let requested = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::RunCancelRequested {
                run_kind,
                run_id,
                actor,
                decision_seq,
            } => Some((*run_kind, run_id.clone(), *actor, *decision_seq)),
            _ => None,
        })
        .expect("a run_cancel_requested event");
    assert_eq!(requested.0, RunKind::Session);
    assert_eq!(requested.1, session.id.to_string());
    assert_eq!(requested.2, Actor::Human);
    assert!(requested.3.is_some());
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::SessionCompleted { status, .. } if *status == SessionStatus::Cancelled
    )));
    let decisions = store
        .decisions(Some(("session", session.id.as_str())), 10)
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].action, DecisionAction::CancelRun);
    assert!(decisions[0].enforced);

    // And the slot came back: cancelling *is* deallocating, once the drain has
    // been woken up to do it.
    wait_until(Duration::from_secs(60), || {
        let pool = pool.clone();
        let vm_id = vm_id.clone();
        async move { pool.pool.get(&vm_id).await.is_none() }
    })
    .await;
}

/// A cancelled scout keeps what it had written down, and the notes say why the
/// last look was called off — so the next attempt reads both.
#[tokio::test]
async fn a_cancelled_scout_keeps_its_salvage_and_stamps_the_reason() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    let gate = tmp.path().join("notes-gate");
    let agent_cmd = format!("{} {}", gated_notes_agent_path().display(), gate.display());
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, &agent_cmd, &workdir_root).await;
    let (_pool, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, TaskState::Queued).await;
    let api = Api::spawn(store.clone()).await;

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(600),
            leases: None,
        },
    );
    let target = ScoutTarget {
        source: tasks::broker::CloneSource::Direct(format!("file://{}", repo.display())),
        base_branch: "main".into(),
    };
    let dispatched = task.clone();
    let dispatch = tokio::spawn(async move { scout.dispatch(dispatched, &target).await });

    wait_for_file(&gate.with_file_name("notes-gate.started")).await;
    let session = store.list_sessions().await.unwrap().pop().expect("session");

    // Wait for the checkpoint to reach the host (the wrapper sets a 1s
    // interval), so what survives is what was streamed rather than what the
    // cancel happened to catch.
    let s = store.clone();
    let id = session.id.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_scout_notes(&id).await.unwrap().is_some() }
    })
    .await;

    let (status, ack) = api
        .cancel(
            &format!("/sessions/{}/cancel", session.id),
            json!({ "rationale": "wrong branch" }),
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert!(matches!(
        dispatch.await.unwrap(),
        Err(ScoutError::Cancelled(_))
    ));

    let notes = store
        .get_scout_notes(&session.id)
        .await
        .unwrap()
        .expect("the streamed checkpoint outlived the cancel");
    assert!(notes.notes.contains("src/parse.rs"), "{}", notes.notes);
    assert_eq!(
        notes.reason.as_deref(),
        Some("cancelled by human: wrong branch"),
        "the cancel stamps its own rationale onto the notes"
    );

    // And the salvage reaches the next attempt: `salvage_for_task` accepts a
    // cancelled session's notes for the same reason it accepts a stopped-early
    // one's — the leads are worth the same either way.
    let carried = store
        .salvage_for_task(&task.id)
        .await
        .unwrap()
        .expect("a cancelled run's notes are still salvage");
    assert_eq!(carried.session_id, session.id);
}

/// A build somebody stopped is `cancelled`, not `failed`, and its batch is
/// handed back untouched — the specs approved, the tasks ready to build.
#[tokio::test]
async fn a_cancelled_build_returns_its_specs_without_a_strike() {
    let supervisor_bin = workspace_bin("builder-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let clone_root = tmp.path().join("repos");
    make_fixture_repo(&clone_root.join("test"), "repo.git").await;
    let workdir_root = tmp.path().join("builder-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    let gate = tmp.path().join("build-gate");
    let agent_cmd = format!(
        "{} {}",
        gated_builder_agent_path().display(),
        gate.display()
    );
    let wrapper =
        write_builder_supervisor_wrapper(tmp.path(), &supervisor_bin, &agent_cmd, &workdir_root)
            .await;
    let (pool, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let project = insert_project(&store).await;
    let (task, spec) = seed_approved(&store, &project).await;
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let claimed = store.claim_next_queued_build().await.unwrap().unwrap();
    let api = Api::spawn(store.clone()).await;

    let builder = Builder::new(
        store.clone(),
        client.handle(),
        // Never reached: a cancelled build never lands a branch, so the PR
        // call this would make is never made.
        Arc::new(GitHubClient::with_base_url(
            "token",
            "http://unused.invalid/graphql",
        )),
        BuilderConfig {
            image: "builder:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(600),
            leases: None,
            scratch_root: tmp.path().join("scratch"),
        },
    );
    let url = format!("file://{}", clone_root.join("test/repo.git").display());
    let dispatch = tokio::spawn(async move {
        builder
            .dispatch(claimed, &tasks::broker::CloneSource::Direct(url))
            .await
    });

    wait_for_file(&gate.with_file_name("build-gate.started")).await;
    let vm_id = VmId::new(
        store
            .get_build(&build.id)
            .await
            .unwrap()
            .unwrap()
            .vm_id
            .expect("the build recorded its VM"),
    );

    let (status, ack) = api
        .cancel(
            &format!("/builds/{}/cancel", build.id),
            json!({ "rationale": "the base branch moved under it" }),
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["run_kind"], "build");
    assert_eq!(ack["concluded"], true, "{ack}");

    assert!(matches!(
        dispatch.await.unwrap(),
        Err(BuilderError::Cancelled(_))
    ));

    let concluded = store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, BuildStatus::Cancelled);
    assert_eq!(
        concluded.exit_reason.as_deref(),
        Some("cancelled by human: the base branch moved under it")
    );
    assert!(concluded.pr_number.is_none(), "nothing was pushed");

    // The work is intact and immediately re-dispatchable — which is also what
    // proves the serial lane is free again.
    assert_eq!(
        store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Approved
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );
    let again = store
        .create_build(&[spec.id], "main", DecisionInput::human())
        .await
        .unwrap();
    assert_eq!(
        store.claim_next_queued_build().await.unwrap().unwrap().id,
        again.id,
        "a cancelled build must not wedge the serial queue"
    );

    wait_until(Duration::from_secs(60), || {
        let pool = pool.clone();
        let vm_id = vm_id.clone();
        async move { pool.pool.get(&vm_id).await.is_none() }
    })
    .await;
}

/// A build nobody has claimed yet has no dispatcher to interrupt, so the
/// request itself has to apply it — otherwise it would sit unread forever.
#[tokio::test]
async fn a_queued_build_is_cancelled_by_the_request_itself() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let (task, spec) = seed_approved(&store, &project).await;
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let api = Api::spawn(store.clone()).await;

    let (status, ack) = api
        .cancel(
            &format!("/builds/{}/cancel", build.id),
            json!({ "rationale": "changed my mind before it started" }),
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["concluded"], true, "{ack}");
    assert_eq!(ack["status"], "cancelled", "{ack}");

    let concluded = store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, BuildStatus::Cancelled);
    // Nothing had moved yet, so nothing needed putting back.
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );
    assert_eq!(
        store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Approved
    );
    // The lane never opened, so it is still open.
    assert!(store.claim_next_queued_build().await.unwrap().is_none());
}

/// The endpoint's refusals, which are where a cancel is cheapest to get wrong:
/// a run that does not exist, one that has already concluded, and an
/// orchestrator that will not say why.
#[tokio::test]
async fn a_cancel_is_refused_when_there_is_nothing_to_stop() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let api = Api::spawn(store.clone()).await;

    let (status, body) = api.cancel("/sessions/sess_nope/cancel", json!({})).await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = api.cancel("/builds/build_nope/cancel", json!({})).await;
    assert_eq!(status, 404, "{body}");

    // A session that already concluded: 409, and nothing recorded — a cancel
    // for a run nobody can stop must not leave a decision row claiming
    // otherwise.
    let task = insert_task(&store, &project, TaskState::InReview).await;
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
    store.insert_session(&session).await.unwrap();
    let (status, body) = api
        .cancel(
            &format!("/sessions/{}/cancel", session.id),
            json!({ "rationale": "too late" }),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("scout_succeeded"),
        "{body}"
    );
    assert!(
        store
            .decisions(Some(("session", session.id.as_str())), 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing is recorded about a run that cannot be stopped"
    );
    assert!(
        store
            .pending_cancel(RunKind::Session, session.id.as_str())
            .await
            .unwrap()
            .is_none()
    );

    // An orchestrator cancel with no rationale is refused before any work is
    // destroyed — which is exactly why the ledger row is written first.
    let running = Session {
        id: SessionId::new(),
        status: SessionStatus::Running,
        completed_at: None,
        ..session
    };
    store.insert_session(&running).await.unwrap();
    let (status, body) = api
        .cancel_as_orchestrator(&format!("/sessions/{}/cancel", running.id), json!({}))
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        store
            .pending_cancel(RunKind::Session, running.id.as_str())
            .await
            .unwrap()
            .is_none(),
        "the refusal must land before the cancel is recorded"
    );

    // With `cancel_runs` off, the same call is refused by the charter and
    // names the capability, so the agent knows what to ask a human for.
    store
        .set_charter(Capability::CancelRuns, CharterLevel::Off, None)
        .await
        .unwrap();
    let (status, body) = api
        .cancel_as_orchestrator(
            &format!("/sessions/{}/cancel", running.id),
            json!({ "rationale": "it is looping" }),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("cancel_runs"),
        "{body}"
    );
}

/// A second cancel for the same run changes nothing: the request on record is
/// the first one, so the actor and rationale that reach `exit_reason` are the
/// ones that actually stopped it.
#[tokio::test]
async fn cancelling_twice_keeps_the_first_request() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, TaskState::Scouting).await;
    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: Some("vm-1".into()),
        branch: String::new(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
        directions: None,
    };
    store.insert_session(&session).await.unwrap();
    let api = Api::spawn(store.clone()).await;

    // Nothing is following this run, so both calls answer `concluded: false` —
    // honest, and the wording deliberately does not claim teardown is underway.
    for rationale in ["first", "second"] {
        let (status, ack) = api
            .cancel(
                &format!("/sessions/{}/cancel", session.id),
                json!({ "rationale": rationale }),
            )
            .await;
        assert_eq!(status, 200, "{ack}");
        assert_eq!(ack["concluded"], false, "{ack}");
        assert_eq!(ack["status"], "running", "{ack}");
    }

    let request = store
        .pending_cancel(RunKind::Session, session.id.as_str())
        .await
        .unwrap()
        .expect("a request on record");
    assert_eq!(request.rationale.as_deref(), Some("first"));
    assert_eq!(request.exit_reason(), "cancelled by human: first");
}

/// The strike accounting, without spending three VMs on it: a cancelled build
/// waives the attempt exactly as an unresumable one does, so three cancels in a
/// row cannot retire a spec nobody has anything against.
#[tokio::test]
async fn three_cancelled_builds_do_not_block_a_spec() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let (task, spec) = seed_approved(&store, &project).await;

    for round in 1..=3 {
        let build = store
            .create_build(
                std::slice::from_ref(&spec.id),
                "main",
                DecisionInput::human(),
            )
            .await
            .unwrap();
        store.claim_next_queued_build().await.unwrap().unwrap();
        store.set_build_vm(&build.id, "vm-allocated").await.unwrap();
        store
            .finalize_build_cancelled(&build.id, "cancelled by human: not now")
            .await
            .unwrap();

        assert_eq!(
            store.get_build(&build.id).await.unwrap().unwrap().status,
            BuildStatus::Cancelled,
            "round {round}"
        );
        assert_eq!(
            store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Approved,
            "round {round}: a cancel says nothing about whether the spec can be built"
        );
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild,
            "round {round}"
        );
    }

    // The counter really is untouched, not merely under the cap: one ordinary
    // failure after three cancels is still the *first* strike.
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();
    store.set_build_vm(&build.id, "vm-allocated").await.unwrap();
    store
        .finalize_build_failed_with(&build.id, "the agent died", Strike::Charge)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Approved,
        "one strike out of three is not blocked"
    );
}

/// `POST /runs/cancel-all` is N single cancels over exactly the set that
/// holds a VM: every `running` session and `running` build gets its own
/// decision row and durable request, and a `queued` build — durable intent,
/// no container — survives untouched.
#[tokio::test]
async fn cancel_all_stops_the_running_set_and_leaves_the_queue_alone() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let api = Api::spawn(store.clone()).await;

    // Nothing running: a real answer, not an error, and nothing recorded.
    let (status, body) = api.cancel("/runs/cancel-all", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["runs"].as_array().unwrap().len(), 0, "{body}");
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("nothing is running"),
        "{body}"
    );

    // One running scout, one claimed (running) build, one queued build — the
    // queued one on a second project only because a task's issue number is
    // unique per project. The scout session rides the seeded task: `in_flight`
    // selects on the session row's own status, so the task needs no second
    // state to make the session count.
    let project = insert_project(&store).await;
    let (task_a, spec_a) = seed_approved(&store, &project).await;
    let session = Session {
        id: SessionId::new(),
        task_id: task_a.id.clone(),
        vm_id: Some("vm-scout".into()),
        branch: "scout/x".into(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
        directions: None,
    };
    store.insert_session(&session).await.unwrap();

    let running_build = store
        .create_build(
            std::slice::from_ref(&spec_a.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();

    let project_b = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "other-repo".into(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project_b).await.unwrap();
    let (_task_b, spec_b) = seed_approved(&store, &project_b).await;
    let queued_build = store
        .create_build(
            std::slice::from_ref(&spec_b.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();

    let (status, body) = api
        .cancel(
            "/runs/cancel-all",
            json!({ "rationale": "clearing the deck" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "the queued build is not a run: {body}");

    // Each run got the full single-cancel treatment: a ledger row and a
    // durable request carrying the shared rationale.
    for (kind, id) in [
        (RunKind::Session, session.id.as_str().to_string()),
        (RunKind::Build, running_build.id.as_str().to_string()),
    ] {
        let request = store
            .pending_cancel(kind, &id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no cancel request for {kind} {id}"));
        assert_eq!(request.rationale.as_deref(), Some("clearing the deck"));
        assert!(
            !store
                .decisions(Some((kind.as_str(), &id)), 10)
                .await
                .unwrap()
                .is_empty(),
            "no decision row for {kind} {id}"
        );
    }

    // The queued build holds no container and must survive exactly as it was.
    let untouched = store.get_build(&queued_build.id).await.unwrap().unwrap();
    assert_eq!(untouched.status, BuildStatus::Queued);
    assert!(
        store
            .pending_cancel(RunKind::Build, queued_build.id.as_str())
            .await
            .unwrap()
            .is_none(),
        "a queued build gets no cancel request from cancel-all"
    );
}
