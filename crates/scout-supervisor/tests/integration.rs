//! Integration tests for scout-supervisor.
//!
//! Spins up the real supervisor binary as a child process, points it at a
//! real local git repo fixture, runs a stub agent (bash script) as the
//! "agent command", and verifies that ScoutEvents stream back correctly.
//! No mocks.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tasks_protocol::{ScoutCommand, ScoutEvent, TasksProtocol};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use vm_pool_protocol::{VmCommand, VmEvent};

type TVmCommand = VmCommand<TasksProtocol>;
type TVmEvent = VmEvent<TasksProtocol>;

/// Build the scout-supervisor binary, returning its path.
async fn build_supervisor() -> PathBuf {
    let output = Command::new("cargo")
        .args(["build", "-p", "scout-supervisor", "--message-format=json"])
        .output()
        .await
        .expect("cargo build");
    assert!(
        output.status.success(),
        "scout-supervisor build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|msg| {
            let reason = msg.get("reason")?.as_str()?;
            let target_name = msg.get("target")?.get("name")?.as_str()?;
            if reason == "compiler-artifact" && target_name == "scout-supervisor" {
                Some(PathBuf::from(msg.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("scout-supervisor binary path in cargo output")
}

/// Create a git repo on disk with an initial commit, return its path.
async fn make_fixture_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("fixture-repo");
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

fn fixture_stub_agent() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("stub-agent.sh")
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
            .env("SCOUT_AGENT_CMD", agent_cmd)
            .env("SCOUT_WORKDIR_ROOT", workdir_root);
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

#[tokio::test]
async fn ping_pong_and_shutdown() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;

    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::Ping).await;
    assert!(matches!(sup.recv().await, VmEvent::Pong));

    sup.send(VmCommand::Shutdown).await;
    assert!(matches!(sup.recv().await, VmEvent::Shutdown));

    sup.close().await;
}

#[tokio::test]
async fn start_scout_completes_with_spec() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());
    let agent = fixture_stub_agent();

    let mut sup = SupervisorProc::spawn(&binary, agent.to_str().unwrap(), tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: ScoutCommand::Start {
            task_id: "task_42".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            prompt: "Implement a stub function.".into(),
        },
    })
    .await;

    // Drain events until we see Completed (or Failed).
    let mut saw_started = false;
    let mut saw_impl_finished = false;
    let mut completion: Option<ScoutEvent> = None;
    while completion.is_none() {
        match sup.recv().await {
            VmEvent::App {
                payload: ScoutEvent::Started { branch },
            } => {
                assert!(branch.starts_with("scout/task_42-"));
                saw_started = true;
            }
            VmEvent::App {
                payload: ScoutEvent::Progress { .. },
            } => {
                // fine
            }
            VmEvent::App {
                payload: ScoutEvent::ImplementationFinished { exit_code },
            } => {
                assert_eq!(exit_code, 0);
                saw_impl_finished = true;
            }
            VmEvent::App {
                payload: evt @ (ScoutEvent::Completed { .. } | ScoutEvent::Failed { .. }),
            } => {
                completion = Some(evt);
            }
            other => panic!("unexpected event before completion: {other:?}"),
        }
    }

    assert!(saw_started, "did not observe Started");
    assert!(saw_impl_finished, "did not observe ImplementationFinished");

    match completion.unwrap() {
        ScoutEvent::Completed {
            spec_markdown,
            files_touched,
        } => {
            assert!(spec_markdown.contains("## Spec"), "spec: {spec_markdown}");
            // The stub echoes its stdin into the spec — proves the prompt
            // actually reached the agent.
            assert!(
                spec_markdown.contains("Implement a stub function."),
                "prompt did not reach the agent via stdin: {spec_markdown}"
            );
            // The stub COMMITS src/stub.rs — proves files_touched diffs
            // against the recorded base SHA, not HEAD.
            assert!(files_touched.contains(&"src/stub.rs".to_string()));
            // SPEC.md and PROMPT.md should be filtered out.
            assert!(!files_touched.iter().any(|f| f == "SPEC.md"));
            assert!(!files_touched.iter().any(|f| f == "PROMPT.md"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    assert!(matches!(sup.recv().await, VmEvent::Shutdown));
    sup.close().await;
}

#[tokio::test]
async fn start_scout_fails_if_agent_missing_spec() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());

    // Agent that runs successfully but produces no SPEC.md
    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: ScoutCommand::Start {
            task_id: "task_7".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            prompt: "n/a".into(),
        },
    })
    .await;

    let mut failure: Option<ScoutEvent> = None;
    while failure.is_none() {
        match sup.recv().await {
            VmEvent::App {
                payload: ScoutEvent::Failed { reason },
            } => {
                failure = Some(ScoutEvent::Failed { reason });
            }
            VmEvent::App { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    match failure.unwrap() {
        ScoutEvent::Failed { reason } => assert!(
            reason.contains("SPEC.md"),
            "expected SPEC.md failure, got: {reason}"
        ),
        _ => unreachable!(),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

#[tokio::test]
async fn start_scout_fails_on_clone_error() {
    let binary = build_supervisor().await;
    let tmp = tempfile::tempdir().unwrap();

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: ScoutCommand::Start {
            task_id: "task_bad".into(),
            repo_clone_url: "file:///nonexistent/repo".into(),
            base_branch: "main".into(),
            prompt: "n/a".into(),
        },
    })
    .await;

    // First event should be a Failed (clone error) — we should not see Started.
    match sup.recv().await {
        VmEvent::App {
            payload: ScoutEvent::Failed { reason },
        } => {
            assert!(reason.contains("clone"), "reason: {reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}
