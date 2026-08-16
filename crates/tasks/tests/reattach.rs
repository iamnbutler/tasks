//! What a `tasks serve` restart costs, end to end.
//!
//! These tests kill a server mid-run the way SIGKILL does — the dispatch
//! future is dropped, leaving a `running` row and a live VM — and then bring a
//! second process up through the same `resume_in_flight` → `reconcile_startup`
//! sequence `run()` uses. Nothing is mocked: a real vm-pool service, real
//! supervisor binaries, a real agent process, a real git remote.
//!
//! The agents are *gated*: they block on a file the test creates, so the run
//! is provably still in flight when the first process dies. A test that could
//! pass by the scout having quietly finished first would prove nothing.
//!
//! Both halves are covered. Reattachment must narrow orphaning, not remove
//! it: a session whose VM really is gone is still written off, and a server
//! that cannot reach vm-pool at all falls back to exactly the old behaviour.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::Json as AxumJson;
use axum::routing::post;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::process::Command;

use tasks::github::{GitHubClient, IntakeFilter};
use tasks::models::{
    Build, BuildStatus, Complexity, DecisionInput, GhState, Project, ProjectId, Session, SessionId,
    SessionStatus, Spec, SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
    TranscriptOwner,
};
use tasks::run::{self, Config, InFlight};
use tasks::scout::{Scout, ScoutConfig, ScoutTarget};
use tasks::store::Store;
use tasks_protocol::{TaskEvent, TasksProtocol};
use vm_pool_client::Client;
use vm_pool_manager::SupervisorRuntime;
use vm_pool_protocol::{VmConfig, VmId};
use vm_pool_service::Service;

mod common;
use common::{
    gated_agent_path, gated_builder_agent_path, make_fixture_repo, spawn_vm_pool, wait_until,
    workspace_bin, write_builder_supervisor_wrapper, write_supervisor_wrapper,
};

type VmPool = Arc<Service<SupervisorRuntime, TasksProtocol>>;

/// A Config pointing at this test's vm-pool socket and local fixture repos.
fn test_config(vm_pool_socket: &Path, data_dir: &Path, clone_root: &Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        port: 0,
        poll_interval: Duration::from_secs(3600),
        startup_mode: tasks::run::DEFAULT_STARTUP_MODE,
        scout_max_concurrent: 1,
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

async fn insert_task(store: &Store, project: &Project, state: TaskState) -> Task {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 1,
        title: "survives a restart".into(),
        body: "body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    task
}

/// Wait until vm-pool's own event log holds a terminal event for `vm_id`.
///
/// This is what makes the replay path deterministic rather than a race: once
/// the outcome is in the log and nobody is connected, the only way the second
/// process can learn it is by attaching and replaying.
async fn wait_for_terminal_event(pool: &VmPool, vm_id: &VmId, terminal: fn(&TaskEvent) -> bool) {
    let pool = pool.clone();
    let vm_id = vm_id.clone();
    wait_until(Duration::from_secs(120), || {
        let pool = pool.clone();
        let vm_id = vm_id.clone();
        async move {
            let (events, _) = pool.events.app_events_for_vm(&vm_id, 0, 512).await;
            events.iter().any(|(_, e)| terminal(e))
        }
    })
    .await;
}

fn scout_concluded(event: &TaskEvent) -> bool {
    matches!(
        event,
        TaskEvent::Scout(
            tasks_protocol::ScoutEvent::Completed { .. }
                | tasks_protocol::ScoutEvent::StoppedEarly { .. }
                | tasks_protocol::ScoutEvent::Failed { .. }
        )
    )
}

fn build_concluded(event: &TaskEvent) -> bool {
    matches!(
        event,
        TaskEvent::Build(
            tasks_protocol::BuildEvent::Completed { .. }
                | tasks_protocol::BuildEvent::Failed { .. }
        )
    )
}

/// The vm_id the session picked up, once it has one.
async fn session_vm_id(store: &Arc<Store>, session_id: &SessionId) -> VmId {
    let s = store.clone();
    let id = session_id.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        let id = id.clone();
        async move {
            s.get_session(&id)
                .await
                .unwrap()
                .is_some_and(|session| session.vm_id.is_some())
        }
    })
    .await;
    VmId::new(
        store
            .get_session(session_id)
            .await
            .unwrap()
            .unwrap()
            .vm_id
            .unwrap(),
    )
}

async fn wait_for_file(path: &Path) {
    let path = path.to_path_buf();
    wait_until(Duration::from_secs(120), || {
        let path = path.clone();
        async move { path.exists() }
    })
    .await;
}

/// The load-bearing test: a scout that was still running when the server died
/// finishes anyway, under the process that came after it.
#[tokio::test]
async fn a_restart_reattaches_to_a_scout_instead_of_orphaning_it() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let clone_root = tmp.path().join("repos");
    make_fixture_repo(&clone_root.join("test"), "repo.git").await;
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    let gate = tmp.path().join("scout-gate");
    let agent_cmd = format!("{} {}", gated_agent_path().display(), gate.display());
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, &agent_cmd, &workdir_root).await;
    let (pool, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;

    let db = tmp.path().join("tasks.db");
    let config = test_config(&socket, tmp.path(), &clone_root);

    // --- the first process ---
    let store = Arc::new(Store::open(&db).await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, TaskState::Queued).await;

    let client = Client::<TasksProtocol>::connect(&socket).await.unwrap();
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
        repo_clone_url: format!("file://{}", clone_root.join("test/repo.git").display()),
        base_branch: "main".into(),
    };
    let task_for_dispatch = task.clone();
    let dispatch = tokio::spawn(async move { scout.dispatch(task_for_dispatch, &target).await });

    // The agent is really running: it says so itself, and it will not conclude
    // until this test lets it.
    wait_for_file(&gate.with_file_name("scout-gate.started")).await;
    let sessions = store.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    let session_id = sessions[0].id.clone();
    let vm_id = session_vm_id(&store, &session_id).await;
    assert_eq!(sessions[0].status, SessionStatus::Running);

    // --- the crash: everything a SIGKILL leaves behind ---
    dispatch.abort();
    drop(client);
    drop(store);

    // The run concludes while there is nobody to hear it. From here on, the
    // only way to learn the outcome is to attach and replay.
    tokio::fs::write(&gate, "go").await.unwrap();
    wait_for_terminal_event(&pool, &vm_id, scout_concluded).await;

    // --- the second process, booting exactly as `run()` does ---
    let store = Arc::new(Store::open(&db).await.unwrap());
    let in_flight = InFlight::default();
    let resumed = run::resume_in_flight(&store, &config, &in_flight).await;
    assert!(
        resumed.sessions.contains(&session_id),
        "the surviving session must be picked up, not reconciled"
    );
    run::reconcile_startup_except(&store, &resumed)
        .await
        .unwrap();

    // Reconciliation left the resumed row alone…
    assert_eq!(
        store
            .get_session(&session_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Running,
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::Scouting,
    );

    // …and the reattach carries the run through to a spec.
    let s = store.clone();
    wait_until(Duration::from_secs(120), || {
        let s = s.clone();
        async move { !s.list_specs().await.unwrap().is_empty() }
    })
    .await;

    let specs = store.list_specs().await.unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].session_id.as_ref(),
        Some(&session_id),
        "the same run, not a new one"
    );
    assert!(specs[0].content.contains("Gated implementation"));

    let session = store.get_session(&session_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::ScoutSucceeded);
    assert!(
        session.branch.starts_with("scout/"),
        "the branch survived the restart on the row: {:?}",
        session.branch
    );

    // The transcript says where the seam is. Replayed output is deliberately
    // not re-persisted — there is no watermark for what the dead process
    // already wrote, so a stated gap beats a silently doubled tail.
    let transcript = store
        .transcript_since(&TranscriptOwner::session(&session_id), 0, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.line)
        .collect::<Vec<_>>();
    assert_eq!(
        transcript
            .iter()
            .filter(|l| l.contains("picked back up after a server restart"))
            .count(),
        1,
        "transcript: {transcript:?}"
    );
    assert!(
        transcript
            .iter()
            .filter(|l| l.contains("[gated-agent] starting in"))
            .count()
            <= 1,
        "replayed output must not be persisted twice: {transcript:?}"
    );

    let stored = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::InReview);
    assert_eq!(
        stored.dispatch_attempts, 0,
        "a restart is not a failed attempt"
    );
    assert_eq!(
        store
            .get_spec_queue_entry(&specs[0].id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::PendingReview
    );

    // The VM was handed back by the process that finished the run.
    wait_until(Duration::from_secs(60), || {
        let pool = pool.clone();
        let vm_id = vm_id.clone();
        async move { pool.pool.get(&vm_id).await.is_none() }
    })
    .await;
}

/// The same for a build — and a build has more at stake, because its branch
/// has no home until the server pushes it.
#[tokio::test]
async fn a_restart_reattaches_to_a_build_and_still_lands_the_branch() {
    let supervisor_bin = workspace_bin("builder-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
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

    // A real git repo standing in for the remote, and a real HTTP server
    // standing in for GitHub's REST API.
    let clone_root = tmp.path().join("repos");
    let repo = make_fixture_repo(&clone_root.join("test"), "repo.git").await;
    let (rest_url, seen_prs) = spawn_fake_github_rest(77).await;

    let db = tmp.path().join("tasks.db");
    let mut config = test_config(&socket, tmp.path(), &clone_root);
    config.github_token = Some("token".into());
    config.github_api_url = Some("http://unused.invalid/graphql".into());
    config.github_rest_api_url = Some(rest_url);

    // --- the first process ---
    let store = Arc::new(Store::open(&db).await.unwrap());
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

    let client = Client::<TasksProtocol>::connect(&socket).await.unwrap();
    let builder = tasks::builder::Builder::new(
        store.clone(),
        client.handle(),
        Arc::new(
            GitHubClient::with_base_url("token", "http://unused.invalid/graphql")
                .with_rest_base_url(config.github_rest_api_url.clone().unwrap()),
        ),
        tasks::builder::BuilderConfig {
            image: "builder:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(300),
            scratch_root: tmp.path().join("scratch"),
        },
    );
    let repo_url = format!("file://{}", clone_root.join("test/repo.git").display());
    let url = repo_url.clone();
    let dispatch = tokio::spawn(async move { builder.dispatch(claimed, &url).await });

    wait_for_file(&gate.with_file_name("build-gate.started")).await;
    let vm_id = VmId::new(
        wait_for_build_vm(&store, &build.id)
            .await
            .vm_id
            .expect("the build recorded its VM"),
    );

    // --- the crash ---
    dispatch.abort();
    drop(client);
    drop(store);

    tokio::fs::write(&gate, "go").await.unwrap();
    wait_for_terminal_event(&pool, &vm_id, build_concluded).await;

    // --- the second process ---
    let store = Arc::new(Store::open(&db).await.unwrap());
    let in_flight = InFlight::default();
    let resumed = run::resume_in_flight(&store, &config, &in_flight).await;
    assert!(resumed.builds.contains(&build.id));
    run::reconcile_startup_except(&store, &resumed)
        .await
        .unwrap();

    let s = store.clone();
    let build_id = build.id.clone();
    wait_until(Duration::from_secs(120), || {
        let s = s.clone();
        let id = build_id.clone();
        async move { s.get_build(&id).await.unwrap().unwrap().status != BuildStatus::Running }
    })
    .await;

    let done = store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(
        done.status,
        BuildStatus::Succeeded,
        "exit_reason: {:?}",
        done.exit_reason
    );
    assert_eq!(done.pr_number, Some(77));

    // The branch REALLY landed: it is in the remote repo, at the reported tip,
    // with the commits the agent made before the restart.
    let head_sha = done.head_sha.clone().expect("head sha");
    let branch_ref = format!("refs/heads/{}", done.branch);
    assert_eq!(run_git(&repo, &["rev-parse", &branch_ref]).await, head_sha);
    let listing = run_git(&repo, &["ls-tree", "-r", "--name-only", &head_sha]).await;
    assert!(listing.contains("src/built.rs"), "tree: {listing}");

    assert_eq!(seen_prs.lock().unwrap().len(), 1);
    assert_eq!(
        store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Built
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::AwaitingMerge
    );
}

/// Reattachment narrows orphaning; it does not remove it. A session whose VM
/// is genuinely gone is still written off, and still costs the task nothing.
#[tokio::test]
async fn a_session_whose_vm_is_gone_is_still_written_off() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmp.path().join("never-runs.sh");
    tokio::fs::write(&wrapper, "#!/bin/sh\nexit 1\n")
        .await
        .unwrap();
    let (_pool, socket) = spawn_vm_pool(tmp.path(), &wrapper, 1).await;
    let config = test_config(&socket, tmp.path(), &tmp.path().join("repos"));

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, TaskState::Scouting).await;
    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        // A VM from a previous life. vm-pool has never heard of it, and has
        // nothing recorded for it either.
        vm_id: Some("vm-from-a-previous-life".into()),
        branch: String::new(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
    };
    store.insert_session(&session).await.unwrap();

    let in_flight = InFlight::default();
    let resumed = run::resume_in_flight(&store, &config, &in_flight).await;
    run::reconcile_startup_except(&store, &resumed)
        .await
        .unwrap();

    // The reattach owns the row, so it — not reconciliation — has to conclude
    // it. That it does so is the invariant the whole design rests on.
    let s = store.clone();
    let id = session.id.clone();
    wait_until(Duration::from_secs(30), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_session(&id).await.unwrap().unwrap().status != SessionStatus::Running }
    })
    .await;

    let concluded = store.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, SessionStatus::ScoutFailed);
    assert!(concluded.completed_at.is_some());

    let s = store.clone();
    let id = task.id.clone();
    wait_until(Duration::from_secs(30), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_task(&id).await.unwrap().unwrap().state == TaskState::Queued }
    })
    .await;
    assert_eq!(
        store
            .get_task(&task.id)
            .await
            .unwrap()
            .unwrap()
            .dispatch_attempts,
        0,
        "a run nobody could pick up is not the task's fault"
    );
}

/// The same for a build. A `running` build nobody concludes wedges the serial
/// queue forever, which is worse than an orphaned session.
#[tokio::test]
async fn a_build_whose_vm_is_gone_fails_without_wedging_the_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmp.path().join("never-runs.sh");
    tokio::fs::write(&wrapper, "#!/bin/sh\nexit 1\n")
        .await
        .unwrap();
    let (_pool, socket) = spawn_vm_pool(tmp.path(), &wrapper, 1).await;
    let mut config = test_config(&socket, tmp.path(), &tmp.path().join("repos"));
    config.github_token = Some("token".into());

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let (task, spec) = seed_approved(&store, &project).await;
    let in_flight = InFlight::default();

    // Three rounds, which is the build-attempt cap. If a run nobody could pick
    // up counted against the spec, the third would leave it `blocked` — a spec
    // retired for something that never had anything to do with it.
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
        store
            .set_build_vm(&build.id, "vm-from-a-previous-life")
            .await
            .unwrap();

        let resumed = run::resume_in_flight(&store, &config, &in_flight).await;
        run::reconcile_startup_except(&store, &resumed)
            .await
            .unwrap();

        let s = store.clone();
        let id = build.id.clone();
        wait_until(Duration::from_secs(30), || {
            let s = s.clone();
            let id = id.clone();
            async move { s.get_build(&id).await.unwrap().unwrap().status != BuildStatus::Running }
        })
        .await;
        assert_eq!(
            store.get_build(&build.id).await.unwrap().unwrap().status,
            BuildStatus::Failed,
            "round {round}"
        );

        // The work is intact: the spec is still approved and its task is back
        // to ready_to_build — never further back.
        assert_eq!(
            store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Approved,
            "round {round}: the spec is not to blame for a restart"
        );
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild,
            "round {round}"
        );
    }

    // And the lane is free: a new build claims immediately.
    let again = store
        .create_build(&[spec.id], "main", DecisionInput::human())
        .await
        .unwrap();
    assert_eq!(
        store.claim_next_queued_build().await.unwrap().unwrap().id,
        again.id
    );
}

/// Degraded path: with no vm-pool to ask, the server cannot know what
/// survived, so it falls back to writing everything off — which is exactly
/// what it did before reattachment existed.
#[tokio::test]
async fn an_unreachable_vm_pool_falls_back_to_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    // Nothing is listening here, and nothing ever will be.
    let config = test_config(
        &tmp.path().join("absent.sock"),
        tmp.path(),
        &tmp.path().join("repos"),
    );

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;
    let task = insert_task(&store, &project, TaskState::Scouting).await;
    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: Some("vm-that-might-well-be-alive".into()),
        branch: String::new(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
    };
    store.insert_session(&session).await.unwrap();

    let in_flight = InFlight::default();
    let resumed = run::resume_in_flight(&store, &config, &in_flight).await;
    assert!(
        resumed.is_empty(),
        "nothing can be claimed without a connection"
    );
    run::reconcile_startup_except(&store, &resumed)
        .await
        .unwrap();

    // Synchronously written off by reconciliation, exactly as before: no
    // reattach owns the row, so nothing is waiting on one.
    let concluded = store.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, SessionStatus::ScoutFailed);
    assert_eq!(
        concluded.exit_reason.as_deref(),
        Some("orphaned by server restart")
    );
    assert_eq!(
        store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::Queued
    );
}

/// The other degraded path, and the one that used to be *worse* than no
/// reattachment at all: vm-pool is a separate daemon, upgraded separately, so
/// a freshly built server routinely meets a service that predates `attach`.
/// Such a service rejects the command at decode time, the client surfaces an
/// ordinary service error, and a reattach — contractually obliged to conclude
/// the row it was handed — writes off a run that was alive and recoverable.
///
/// The gate has to sit *before* any row is claimed, which is what the last
/// assertion here pins down: `attach` is never sent at all.
#[tokio::test]
async fn a_vm_pool_that_predates_attach_falls_back_to_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    let (socket, commands) = spawn_pre_attach_vm_pool(tmp.path()).await;
    let mut config = test_config(&socket, tmp.path(), &tmp.path().join("repos"));
    config.github_token = Some("token".into());

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = insert_project(&store).await;

    // A scout in flight…
    let scouting = insert_task(&store, &project, TaskState::Scouting).await;
    let session = Session {
        id: SessionId::new(),
        task_id: scouting.id.clone(),
        vm_id: Some("vm-still-scouting".into()),
        branch: String::new(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
    };
    store.insert_session(&session).await.unwrap();

    // …and a build in flight, which has more at stake.
    let (building, spec) = seed_approved(&store, &project).await;
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();
    store
        .set_build_vm(&build.id, "vm-still-building")
        .await
        .unwrap();

    let in_flight = InFlight::default();
    let resumed = run::resume_in_flight(&store, &config, &in_flight).await;
    assert!(
        resumed.is_empty(),
        "a pool that cannot decode attach must have nothing claimed against it"
    );
    run::reconcile_startup_except(&store, &resumed)
        .await
        .unwrap();

    // Written off synchronously by reconciliation, exactly as a server with no
    // reattachment at all did. The exit reason is the tell: "attach failed: …"
    // would mean the server claimed the row and then killed the run itself.
    let concluded = store.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, SessionStatus::ScoutFailed);
    assert_eq!(
        concluded.exit_reason.as_deref(),
        Some("orphaned by server restart")
    );
    assert_eq!(
        store.get_task(&scouting.id).await.unwrap().unwrap().state,
        TaskState::Queued
    );

    let concluded = store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(concluded.status, BuildStatus::Failed);
    assert_eq!(
        concluded.exit_reason.as_deref(),
        Some("orphaned by server restart")
    );

    // And the work itself is intact: the spec is still approved and its task
    // is back in the build queue, not blamed for the operator's skew.
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
        store.get_task(&building.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );

    // The crux. Not "attach failed gracefully" — attach was never sent, which
    // is the only version of this that cannot cost a run.
    let asked = commands.lock().unwrap().clone();
    assert!(
        asked.iter().any(|c| c == "status"),
        "the gate is one status round trip: {asked:?}"
    );
    assert!(
        !asked.iter().any(|c| c == "attach"),
        "nothing may be claimed before the pool says it speaks attach: {asked:?}"
    );
}

// --- helpers ---

/// A vm-pool from before `attach` existed, as raw bytes on a real socket.
///
/// Deliberately not a mock: what is under test is compatibility with a binary
/// this tree no longer contains, so the wire form it emitted is the only
/// honest thing left to test against. It answers `status` in the old shape
/// (no `protocol_version`), and rejects everything else the way the service's
/// read loop does — serde's own message, correlated to the request id, which
/// is why this failure showed up as a killed run rather than as a hang.
///
/// Returns the socket path and the list of command types it was asked for.
async fn spawn_pre_attach_vm_pool(dir: &Path) -> (std::path::PathBuf, Arc<Mutex<Vec<String>>>) {
    let socket = dir.join("pre-attach.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let recorded = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = recorded.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let request: Value = serde_json::from_str(line.trim()).unwrap();
                    let id = &request["id"];
                    let command = request["command"]["type"].as_str().unwrap_or_default();
                    recorded.lock().unwrap().push(command.to_string());

                    let event = if command == "status" {
                        json!({
                            "type": "pool_status",
                            "total": 3, "available": 2, "allocated": 1,
                        })
                    } else {
                        json!({
                            "type": "error",
                            "message": format!(
                                "invalid request: unknown variant `{command}`, expected one of \
                                 `allocate`, `deallocate`, `send`, `snapshot`, `restore`, \
                                 `status`, `tail_logs`, `subscribe_logs`, `unsubscribe_logs` \
                                 at line 1 column 1"
                            ),
                        })
                    };
                    let reply = json!({ "id": id, "event": event }).to_string();
                    writer.write_all(reply.as_bytes()).await.unwrap();
                    writer.write_all(b"\n").await.unwrap();
                    writer.flush().await.unwrap();
                    line.clear();
                }
            });
        }
    });

    (socket, seen)
}

async fn wait_for_build_vm(store: &Arc<Store>, build_id: &tasks::models::BuildId) -> Build {
    let s = store.clone();
    let id = build_id.clone();
    wait_until(Duration::from_secs(60), || {
        let s = s.clone();
        let id = id.clone();
        async move { s.get_build(&id).await.unwrap().unwrap().vm_id.is_some() }
    })
    .await;
    store.get_build(build_id).await.unwrap().unwrap()
}

/// A ready_to_build task with its session, spec, and approved queue entry.
async fn seed_approved(store: &Store, project: &Project) -> (Task, Spec) {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 7,
        title: "wants building".into(),
        body: "issue body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::ReadyToBuild,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
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

async fn spawn_fake_github_rest(number: u64) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let state = seen.clone();
    let app =
        axum::Router::new()
            .route(
                "/repos/{owner}/{repo}/pulls",
                post(
                    move |State(s): State<Arc<Mutex<Vec<Value>>>>,
                          AxumJson(body): AxumJson<Value>| async move {
                        s.lock().unwrap().push(body);
                        AxumJson(json!({ "number": number }))
                    },
                ),
            )
            .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

async fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
