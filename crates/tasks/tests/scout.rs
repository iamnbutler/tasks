//! End-to-end scout dispatch integration test.
//!
//! Spins up:
//! - A real vm-pool-service backed by SupervisorRuntime pointing at the real
//!   scout-supervisor binary (compiled via cargo).
//! - A real vm-pool-client connected over a Unix socket.
//! - A real SQLite store on disk.
//! - A real local git repo fixture + stub-agent shell script.
//!
//! Then calls `Scout::dispatch(task)` and asserts the Spec + queue entry
//! land in the store with the expected state transitions. No mocks.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::process::Command;
use vm_pool_manager::{PoolConfig, SupervisorRuntime};
use vm_pool_protocol::VmConfig;
use vm_pool_service::{Service, ServiceConfig};

use tasks::models::{
    GhState, Project, ProjectId, SessionStatus, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::scout::{Scout, ScoutConfig};
use tasks::store::Store;
use tasks_protocol::TasksProtocol;
use vm_pool_client::Client;

/// Build a cargo target by package name and return its executable path.
async fn cargo_build(package: &str) -> PathBuf {
    let output = Command::new("cargo")
        .args(["build", "-p", package, "--message-format=json"])
        .output()
        .await
        .expect("cargo build");
    assert!(
        output.status.success(),
        "cargo build -p {package} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|msg| {
            let reason = msg.get("reason")?.as_str()?;
            let target_name = msg.get("target")?.get("name")?.as_str()?;
            if reason == "compiler-artifact" && target_name == package {
                Some(PathBuf::from(msg.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("binary path in cargo output")
}

/// Create a git repo on disk with an initial commit, return its path.
async fn make_fixture_repo(base: &Path, name: &str) -> PathBuf {
    let repo = base.join(name);
    tokio::fs::create_dir_all(&repo).await.unwrap();

    let run = |args: &[&str]| {
        let repo = repo.clone();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        async move {
            let status = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .status()
                .await
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        }
    };

    run(&["init", "-b", "main"]).await;
    run(&["config", "user.email", "test@example.com"]).await;
    run(&["config", "user.name", "Test"]).await;
    tokio::fs::write(repo.join("README.md"), "# fixture\n")
        .await
        .unwrap();
    run(&["add", "."]).await;
    run(&["commit", "-m", "init"]).await;
    repo
}

/// Write a wrapper shell script that sets the supervisor's env vars and exec's
/// the real binary. Lets each test use its own agent command / workdir without
/// mutating process-wide env.
async fn write_supervisor_wrapper(
    dir: &Path,
    supervisor_bin: &Path,
    agent_cmd: &str,
    workdir_root: &Path,
) -> PathBuf {
    let wrapper = dir.join("supervisor-wrapper.sh");
    let script = format!(
        "#!/bin/sh\n\
         export SCOUT_AGENT_CMD={agent}\n\
         export SCOUT_WORKDIR_ROOT={root}\n\
         exec {bin}\n",
        agent = shell_escape(agent_cmd),
        root = shell_escape(&workdir_root.display().to_string()),
        bin = shell_escape(&supervisor_bin.display().to_string()),
    );
    tokio::fs::write(&wrapper, script).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = tokio::fs::metadata(&wrapper).await.unwrap().permissions();
        p.set_mode(0o755);
        tokio::fs::set_permissions(&wrapper, p).await.unwrap();
    }
    wrapper
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

fn stub_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("scout-supervisor")
        .join("tests")
        .join("fixtures")
        .join("stub-agent.sh")
}

/// Start a vm-pool Service on a fresh Unix socket. Returns the service + socket path.
async fn spawn_vm_pool(
    tmp: &Path,
    supervisor_wrapper: &Path,
) -> (Arc<Service<SupervisorRuntime, TasksProtocol>>, PathBuf) {
    let socket = tmp.join("vm-pool.sock");
    let snapshot_dir = tmp.join("snapshots");
    let config = ServiceConfig {
        socket_path: socket.clone(),
        snapshot_dir,
        pool: PoolConfig {
            max_vms: 2,
            health_check_interval: 60,
            vm_timeout: 300,
        },
    };
    let runtime = SupervisorRuntime::new(supervisor_wrapper);
    let service: Arc<Service<SupervisorRuntime, TasksProtocol>> =
        Service::<SupervisorRuntime, TasksProtocol>::with_runtime(config, runtime)
            .await
            .expect("service");
    let svc = service.clone();
    tokio::spawn(async move {
        let _ = svc.run().await;
    });

    // Wait for the socket to appear
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists(), "vm-pool socket did not appear");
    (service, socket)
}

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
        state: TaskState::New,
        priority: 0,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    (project, task)
}

#[tokio::test]
async fn scout_dispatch_end_to_end_produces_spec() {
    // 1. Build binaries
    let supervisor_bin = cargo_build("scout-supervisor").await;

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
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    // 4. Set up store with a task
    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) =
        insert_project_and_task(&store, "Stub task", "Do the stub thing").await;

    // 5. Dispatch
    let scout_config = ScoutConfig {
        image: "agent:v1".into(),
        vm_config: VmConfig::default(),
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };
    let mut scout = Scout::new(store.clone(), client, scout_config);
    let spec = scout.dispatch(task.clone()).await.expect("dispatch");

    // 6. Assertions
    assert!(spec.content.contains("## Spec"), "spec content: {}", spec.content);
    assert!(spec.files_touched.iter().any(|f| f == "src/stub.rs"));

    let stored_task = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(stored_task.state, TaskState::SpecReady);

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
            tasks::events::EventPayload::TaskStateChanged { from, to, .. } => {
                Some((*from, *to))
            }
            _ => None,
        })
        .collect();
    assert!(state_changes.contains(&(TaskState::New, TaskState::Scouting)));
    assert!(state_changes.contains(&(TaskState::Scouting, TaskState::SpecReady)));
}

#[tokio::test]
async fn scout_dispatch_failure_resets_task_to_new() {
    let supervisor_bin = cargo_build("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();

    // Agent = `true` → exits cleanly but writes no SPEC.md → ScoutEvent::Failed
    let wrapper =
        write_supervisor_wrapper(tmp.path(), &supervisor_bin, "true", &workdir_root).await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) =
        insert_project_and_task(&store, "Will fail", "No SPEC.md produced").await;

    let scout_config = ScoutConfig {
        image: "agent:v1".into(),
        vm_config: VmConfig::default(),
        repo_clone_url: repo_url,
        base_branch: "main".into(),
    };
    let mut scout = Scout::new(store.clone(), client, scout_config);
    let result = scout.dispatch(task.clone()).await;
    assert!(result.is_err(), "expected dispatch to error, got {result:?}");

    let stored_task = store.get_task(&task.id).await.unwrap().unwrap();
    assert_eq!(
        stored_task.state,
        TaskState::New,
        "failed scout should reset task to New for retry"
    );
}
