//! End-to-end Builder dispatch integration test.
//!
//! Spins up:
//! - A real vm-pool-service pointing at the real builder-supervisor binary.
//! - A real vm-pool-client over a Unix socket.
//! - A real SQLite store.
//! - A real local git repo as the "remote" (non-bare — pushing works for any
//!   branch that isn't the checked-out one, which is what makes a no-mock
//!   push test possible at all).
//! - A real HTTP server standing in for GitHub's REST API, recording the PR
//!   request it receives.
//!
//! Then runs `Builder::dispatch(build, clone_url)` and asserts the branch
//! actually lands in the remote and the store drains the batch. No mocks.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::Json as AxumJson;
use axum::routing::post;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::process::Command;

use tasks::builder::{Builder, BuilderConfig};
use tasks::github::GitHubClient;
use tasks::models::{
    BuildStatus, Complexity, GhState, Project, ProjectId, Session, SessionId, SessionStatus, Spec,
    SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::store::Store;
use tasks_protocol::TasksProtocol;
use vm_pool_client::Client;
use vm_pool_protocol::VmConfig;

mod common;
use common::{
    make_fixture_repo, spawn_vm_pool, stub_builder_agent_path, workspace_bin,
    write_builder_supervisor_wrapper,
};

async fn run_git(dir: &std::path::Path, args: &[&str]) -> String {
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

/// Fake GitHub REST endpoint: answers POST /repos/{owner}/{repo}/pulls with
/// a fixed PR number and records the request body.
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

/// Seed a project plus the full chain a build consumes: a ready_to_build
/// task, its scout session, its spec, and an approved queue entry.
async fn seed_approved(store: &Store, project: &Project, issue: u64, title: &str) -> (Task, Spec) {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: issue,
        title: title.into(),
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
        session_id: session.id,
        task_id: task.id.clone(),
        content: format!("## Spec: {title}\n\nAdd a function for issue {issue}."),
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

struct Harness {
    store: Arc<Store>,
    builder: Builder,
    project: Project,
    repo_url: String,
    repo: std::path::PathBuf,
    seen_prs: Arc<Mutex<Vec<Value>>>,
    _tmp: tempfile::TempDir,
}

async fn harness(agent_cmd: &str) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let supervisor_bin = workspace_bin("builder-supervisor").await;
    let wrapper =
        write_builder_supervisor_wrapper(tmp.path(), &supervisor_bin, agent_cmd, tmp.path()).await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 1).await;
    let client = Client::<TasksProtocol>::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();

    let repo = make_fixture_repo(tmp.path(), "remote-repo").await;
    let repo_url = format!("file://{}", repo.display());

    let (rest_url, seen_prs) = spawn_fake_github_rest(42).await;
    let github = Arc::new(
        GitHubClient::with_base_url("token", "http://unused.invalid/graphql")
            .with_rest_base_url(rest_url),
    );

    let builder = Builder::new(
        store.clone(),
        client.handle(),
        github,
        BuilderConfig {
            image: "builder:test".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(60),
            scratch_root: tmp.path().join("scratch"),
        },
    );

    Harness {
        store,
        builder,
        project,
        repo_url,
        repo,
        seen_prs,
        _tmp: tmp,
    }
}

#[tokio::test]
async fn a_batch_of_two_specs_lands_as_one_branch_and_one_pr() {
    let h = harness(stub_builder_agent_path().to_str().unwrap()).await;
    let (task_a, spec_a) = seed_approved(&h.store, &h.project, 7, "First thing").await;
    let (task_b, spec_b) = seed_approved(&h.store, &h.project, 9, "Second thing").await;

    let build = h
        .store
        .create_build(&[spec_a.id.clone(), spec_b.id.clone()], "main")
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();
    assert_eq!(claimed.id, build.id);

    let done = h.builder.dispatch(claimed, &h.repo_url).await.unwrap();
    assert_eq!(done.status, BuildStatus::Succeeded);
    assert_eq!(done.pr_number, Some(42));
    let head_sha = done.head_sha.clone().expect("head sha recorded");
    assert!(done.base_sha.is_some(), "base sha recorded from Started");
    assert!(
        done.files_touched.contains(&"src/built.rs".to_string()),
        "files: {:?}",
        done.files_touched
    );

    // The branch REALLY landed: the remote repo has it, at the reported tip.
    let branch_ref = format!("refs/heads/{}", done.branch);
    let tip = run_git(&h.repo, &["rev-parse", &branch_ref]).await;
    assert_eq!(tip, head_sha);
    let listing = run_git(&h.repo, &["ls-tree", "-r", "--name-only", &tip]).await;
    assert!(listing.contains("src/built.rs"));
    assert!(!listing.contains("PROMPT.md") && !listing.contains("SUMMARY.md"));

    // The PR request the fake GitHub saw. Block-scoped (not `drop()`) so the
    // guard is provably not held across the awaits below.
    {
        let prs = h.seen_prs.lock().unwrap();
        assert_eq!(prs.len(), 1);
        let pr = &prs[0];
        assert_eq!(pr["head"], json!(done.branch));
        assert_eq!(pr["base"], json!("main"));
        assert_eq!(pr["title"], json!("Build: #7, #9"));
        let body = pr["body"].as_str().unwrap();
        assert!(body.contains("Implements #7") && body.contains("Implements #9"));
        assert!(
            !body.contains("Closes"),
            "closing an issue is not ours to write"
        );
    }

    // The batch drained: specs built, tasks done.
    for (task, spec) in [(&task_a, &spec_a), (&task_b, &spec_b)] {
        assert_eq!(
            h.store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Built
        );
        assert_eq!(
            h.store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::Done
        );
    }
}

#[tokio::test]
async fn a_failed_build_returns_the_work_without_wedging_the_queue() {
    // `true` as the agent: exits 0 having committed nothing -> empty branch.
    let h = harness("true").await;
    let (task, spec) = seed_approved(&h.store, &h.project, 7, "First thing").await;

    let build = h
        .store
        .create_build(std::slice::from_ref(&spec.id), "main")
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();

    let err = h.builder.dispatch(claimed, &h.repo_url).await.unwrap_err();
    assert!(format!("{err}").contains("no commits"), "{err}");

    let after = h.store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(after.status, BuildStatus::Failed);
    assert!(
        after
            .exit_reason
            .as_deref()
            .unwrap_or("")
            .contains("no commits"),
        "exit_reason: {:?}",
        after.exit_reason
    );
    // Spec stays approved, task returns to ready_to_build — never further
    // back — and the queue is claimable again.
    assert_eq!(
        h.store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Approved
    );
    assert_eq!(
        h.store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );
    assert!(
        h.seen_prs.lock().unwrap().is_empty(),
        "no PR for a failed build"
    );

    let again = h.store.create_build(&[spec.id], "main").await.unwrap();
    assert_eq!(
        h.store.claim_next_queued_build().await.unwrap().unwrap().id,
        again.id
    );
}
