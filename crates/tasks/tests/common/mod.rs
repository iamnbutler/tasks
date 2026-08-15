//! Shared harness for the integration tests: real cargo-built binaries, real
//! git repos, a real vm-pool service. No mocks — see CLAUDE.md.
#![allow(dead_code)] // each test file uses a subset

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::process::Command;
use vm_pool_manager::{PoolConfig, SupervisorRuntime};
use vm_pool_service::{Service, ServiceConfig};

use tasks_protocol::TasksProtocol;

/// Binaries this suite has already located, keyed by package name. The cell
/// is what makes concurrent callers for the same binary wait on one build
/// instead of racing several.
static WORKSPACE_BINS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::OnceCell<PathBuf>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Path to a binary from another workspace package.
///
/// These tests exec binaries they do not own, so `CARGO_BIN_EXE_*` is not
/// available to them. `make test` prebuilds them and exports
/// `TASKS_TEST_BIN_DIR`; this reads that. Without it — a bare `cargo test
/// --workspace` — it falls back to building, once per binary per test
/// process.
pub async fn workspace_bin(package: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("TASKS_TEST_BIN_DIR") {
        let candidate = Path::new(&dir).join(package);
        if candidate.is_file() {
            return candidate;
        }
        // A stale export degrades to a build rather than failing the suite.
        eprintln!("warning: TASKS_TEST_BIN_DIR={dir} has no {package}; building it");
    }

    // Hold the std mutex only long enough to clone the cell out — never
    // across the await below.
    let cell = {
        let mut bins = WORKSPACE_BINS.lock().unwrap();
        bins.entry(package.to_string()).or_default().clone()
    };
    cell.get_or_init(|| cargo_build(package)).await.clone()
}

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
pub async fn make_fixture_repo(base: &Path, name: &str) -> PathBuf {
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
pub async fn write_supervisor_wrapper(
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

pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// A [`Config`] with nothing external wired up — no GitHub token, no vm-pool
/// worth talking to. Enough for the loops that only read `github_client()`
/// (which is then `None`, so briefs stay DB-derived) and `scout_base_branch`.
/// Tests that need real dispatch build their own; see `tests/run.rs`.
pub fn offline_config(data_dir: &Path) -> tasks::run::Config {
    tasks::run::Config {
        data_dir: data_dir.to_path_buf(),
        port: 0,
        poll_interval: Duration::from_secs(3600),
        scout_max_concurrent: 1,
        scout_image: "agent:v1".into(),
        scout_timeout: Duration::from_secs(300),
        vm_pool_socket: data_dir.join("vm-pool.sock"),
        github_token: None,
        github_api_url: None,
        intake: tasks::github::IntakeFilter::All,
        clone_url_base: "https://github.com".into(),
        scout_base_branch: "main".into(),
        vm_config: Default::default(),
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

pub fn stub_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("scout-supervisor")
        .join("tests")
        .join("fixtures")
        .join("stub-agent.sh")
}

/// A stand-in agent that copies its whole stdin prompt into SPEC.md, so a test
/// can assert on what the scout was told.
pub fn echo_prompt_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo-prompt-agent.sh")
}

/// A stand-in agent that emits stream-json shaped output, paced so a test can
/// attach to the live transcript tail mid-run.
pub fn stream_json_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("scout-supervisor")
        .join("tests")
        .join("fixtures")
        .join("stub-agent-stream-json.sh")
}

/// Start a vm-pool Service on a fresh Unix socket. Returns the service + socket path.
pub async fn spawn_vm_pool(
    tmp: &Path,
    supervisor_wrapper: &Path,
    max_vms: usize,
) -> (Arc<Service<SupervisorRuntime, TasksProtocol>>, PathBuf) {
    let socket = tmp.join("vm-pool.sock");
    let snapshot_dir = tmp.join("snapshots");
    let config = ServiceConfig {
        socket_path: socket.clone(),
        snapshot_dir,
        pool: PoolConfig {
            max_vms,
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

/// Poll `check` until it returns true or `timeout` elapses.
pub async fn wait_until<F, Fut>(timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Like [`write_supervisor_wrapper`], for the builder-supervisor's env.
pub async fn write_builder_supervisor_wrapper(
    dir: &Path,
    supervisor_bin: &Path,
    agent_cmd: &str,
    workdir_root: &Path,
) -> PathBuf {
    let wrapper = dir.join("builder-supervisor-wrapper.sh");
    let script = format!(
        "#!/bin/sh\n\
         export BUILDER_AGENT_CMD={agent}\n\
         export BUILDER_WORKDIR_ROOT={root}\n\
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

/// A stand-in builder agent that commits work, forgets one file, and writes
/// SUMMARY.md — lives in the builder-supervisor crate's fixtures.
pub fn stub_builder_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("builder-supervisor")
        .join("tests")
        .join("fixtures")
        .join("stub-builder-agent.sh")
}
