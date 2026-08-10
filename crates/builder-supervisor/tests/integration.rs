//! Integration tests for builder-supervisor.
//!
//! Spins up the real supervisor binary as a child process, points it at a
//! real local git repo fixture, runs a stub agent as the "agent command", and
//! verifies BuildEvents — including that the returned bundle actually
//! reconstructs the branch in a scratch repo. No mocks.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use tasks_protocol::{
    BuildCommand, BuildEvent, ScoutCommand, TaskCommand, TaskEvent, TasksProtocol,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use vm_pool_protocol::{VmCommand, VmEvent};

type TVmCommand = VmCommand<TasksProtocol>;
type TVmEvent = VmEvent<TasksProtocol>;

async fn build_supervisor() -> PathBuf {
    let output = Command::new("cargo")
        .args(["build", "-p", "builder-supervisor", "--message-format=json"])
        .output()
        .await
        .expect("cargo build");
    assert!(
        output.status.success(),
        "builder-supervisor build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|msg| {
            let reason = msg.get("reason")?.as_str()?;
            let target_name = msg.get("target")?.get("name")?.as_str()?;
            if reason == "compiler-artifact" && target_name == "builder-supervisor" {
                Some(PathBuf::from(msg.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("builder-supervisor binary path in cargo output")
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

async fn make_fixture_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("fixture-repo");
    tokio::fs::create_dir_all(&repo).await.unwrap();
    run_git(&repo, &["init", "-b", "main"]).await;
    run_git(&repo, &["config", "user.email", "test@example.com"]).await;
    run_git(&repo, &["config", "user.name", "Test"]).await;
    tokio::fs::write(repo.join("README.md"), "# fixture\n")
        .await
        .unwrap();
    run_git(&repo, &["add", "."]).await;
    run_git(&repo, &["commit", "-m", "init"]).await;
    repo
}

fn fixture_agent() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("stub-builder-agent.sh")
}

struct SupervisorProc {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl SupervisorProc {
    async fn spawn(binary: &Path, agent_cmd: &str, workdir_root: &Path) -> Self {
        let mut cmd = Command::new(binary);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("BUILDER_AGENT_CMD", agent_cmd)
            .env("BUILDER_WORKDIR_ROOT", workdir_root);
        let mut child = cmd.spawn().expect("spawn supervisor");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdout_lines = BufReader::new(stdout).lines();
        Self {
            child,
            stdin,
            stdout_lines,
        }
    }

    async fn send(&mut self, cmd: TVmCommand) {
        let json = serde_json::to_string(&cmd).unwrap();
        self.stdin.write_all(json.as_bytes()).await.unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn recv(&mut self) -> TVmEvent {
        let line = timeout(Duration::from_secs(30), self.stdout_lines.next_line())
            .await
            .expect("recv timeout")
            .expect("stdout read")
            .expect("stdout line");
        serde_json::from_str(&line).expect("parse event")
    }

    async fn close(mut self) {
        drop(self.stdin);
        let _ = self.child.wait().await;
    }
}

fn start(repo_url: String) -> TVmCommand {
    VmCommand::App {
        payload: TaskCommand::Build(BuildCommand::Start {
            build_id: "build_1".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            branch: "build/build_1".into(),
            prompt: "## Spec 1 of 1: do the thing".into(),
        }),
    }
}

/// Drain until a terminal event, asserting the lifecycle shape on the way.
async fn drain(sup: &mut SupervisorProc) -> BuildEvent {
    loop {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Build(evt),
            } => match evt {
                BuildEvent::Started { .. }
                | BuildEvent::Progress { .. }
                | BuildEvent::ImplementationFinished { .. } => continue,
                terminal @ (BuildEvent::Completed { .. } | BuildEvent::Failed { .. }) => {
                    return terminal;
                }
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_build_ships_a_bundle_that_reconstructs_the_branch() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());
    let base_tip = run_git(&repo, &["rev-parse", "main"]).await;

    let agent = fixture_agent();
    let mut sup = SupervisorProc::spawn(&binary, agent.to_str().unwrap(), tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(start(repo_url.clone())).await;

    let (base_sha, head_sha, bundle_base64, summary, files) = match drain(&mut sup).await {
        BuildEvent::Completed {
            base_sha,
            head_sha,
            bundle_base64,
            summary,
            files_touched,
        } => (base_sha, head_sha, bundle_base64, summary, files_touched),
        other => panic!("expected Completed, got {other:?}"),
    };

    assert_eq!(base_sha, base_tip, "branch grew from the fixture tip");
    assert_ne!(head_sha, base_sha);
    // The committed file, the swept file — and neither artifact.
    assert!(files.contains(&"src/built.rs".to_string()));
    assert!(
        files.contains(&"src/forgotten.rs".to_string()),
        "the sweep must commit work the agent forgot: {files:?}"
    );
    assert!(!files.iter().any(|f| f == "PROMPT.md" || f == "SUMMARY.md"));
    assert!(summary.expect("summary").contains("Implemented per spec"));

    // The no-mock proof: unbundle into a scratch repo exactly as the server
    // will, and the branch tip must be the reported head.
    let scratch = tmp.path().join("scratch.git");
    tokio::fs::create_dir_all(&scratch).await.unwrap();
    run_git(&scratch, &["init", "--bare"]).await;
    run_git(&scratch, &["fetch", &repo_url, "main:refs/heads/main"]).await;
    let bundle_path = tmp.path().join("egress.bundle");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(bundle_base64)
        .expect("valid base64");
    tokio::fs::write(&bundle_path, bytes).await.unwrap();
    run_git(
        &scratch,
        &[
            "fetch",
            bundle_path.to_str().unwrap(),
            "refs/heads/build/build_1:refs/heads/build/build_1",
        ],
    )
    .await;
    let tip = run_git(&scratch, &["rev-parse", "refs/heads/build/build_1"]).await;
    assert_eq!(tip, head_sha, "bundle reconstructs the reported head");

    // The sweep commit's tree contains the forgotten file.
    let listing = run_git(&scratch, &["ls-tree", "-r", "--name-only", &tip]).await;
    assert!(listing.contains("src/forgotten.rs"));
    assert!(!listing.contains("PROMPT.md"));
    assert!(!listing.contains("SUMMARY.md"));

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

/// An agent that runs fine but commits nothing produced no build. `head ==
/// base` is the Builder's missing-SPEC.md.
#[tokio::test]
async fn an_empty_branch_is_a_failure() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start(repo_url)).await;

    match drain(&mut sup).await {
        BuildEvent::Failed { reason } => {
            assert!(reason.contains("no commits"), "reason: {reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

#[tokio::test]
async fn a_clone_error_fails_before_started() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start("file:///nonexistent/repo".into())).await;

    match sup.recv().await {
        VmEvent::App {
            payload: TaskEvent::Build(BuildEvent::Failed { reason }),
        } => assert!(reason.contains("clone"), "reason: {reason}"),
        other => panic!("expected Failed, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

/// The wire barrier's supervisor half: a Scout command sent to a Builder VM
/// is refused with a terminal Failed, never acted on.
#[tokio::test]
async fn a_scout_command_is_refused_not_acted_on() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: TaskCommand::Scout(ScoutCommand::Start {
            task_id: "task_x".into(),
            repo_clone_url: "file:///nowhere".into(),
            base_branch: "main".into(),
            prompt: "n/a".into(),
        }),
    })
    .await;

    match sup.recv().await {
        VmEvent::App {
            payload: TaskEvent::Build(BuildEvent::Failed { reason }),
        } => assert!(reason.contains("builder"), "reason: {reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}
