//! Integration tests for scout-supervisor.
//!
//! Spins up the real supervisor binary as a child process, points it at a
//! real local git repo fixture, runs a stub agent (bash script) as the
//! "agent command", and verifies that ScoutEvents stream back correctly.
//! No mocks.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tasks_protocol::{
    BuildCommand, ScoutCommand, ScoutEvent, TaskCommand, TaskEvent, TasksProtocol,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use vm_pool_protocol::{VmCommand, VmEvent};

type TVmCommand = VmCommand<TasksProtocol>;
type TVmEvent = VmEvent<TasksProtocol>;

/// Path to the scout-supervisor binary. Cargo builds it for us as a
/// dependency of this test target and hands over the path — tests exec
/// binaries, they never build them.
fn supervisor_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scout-supervisor"))
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

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture_stub_agent() -> PathBuf {
    fixture("stub-agent.sh")
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
    let binary = supervisor_bin();
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
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());
    let agent = fixture_stub_agent();

    let mut sup = SupervisorProc::spawn(&binary, agent.to_str().unwrap(), tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: TaskCommand::Scout(ScoutCommand::Start {
            task_id: "task_42".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            prompt: "Implement a stub function.".into(),
        }),
    })
    .await;

    // Drain events until we see Completed (or Failed).
    let mut saw_started = false;
    let mut saw_impl_finished = false;
    let mut completion: Option<ScoutEvent> = None;
    while completion.is_none() {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Scout(ScoutEvent::Started { branch }),
            } => {
                assert!(branch.starts_with("scout/task_42-"));
                saw_started = true;
            }
            VmEvent::App {
                payload: TaskEvent::Scout(ScoutEvent::Progress { .. }),
            } => {
                // fine
            }
            VmEvent::App {
                payload: TaskEvent::Scout(ScoutEvent::ImplementationFinished { exit_code }),
            } => {
                assert_eq!(exit_code, 0);
                saw_impl_finished = true;
            }
            VmEvent::App {
                payload:
                    TaskEvent::Scout(evt @ (ScoutEvent::Completed { .. } | ScoutEvent::Failed { .. })),
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
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());

    // Agent that runs successfully but produces no SPEC.md
    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: TaskCommand::Scout(ScoutCommand::Start {
            task_id: "task_7".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            prompt: "n/a".into(),
        }),
    })
    .await;

    let mut failure: Option<ScoutEvent> = None;
    while failure.is_none() {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Scout(ScoutEvent::Failed { reason }),
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

/// An agent the kernel kills — the OOM shape. The exit code must be
/// `128 + 9`, not the `-1` a signal death used to flatten into, and the
/// failure reason must name the signal: for a Scout, that reason is the whole
/// postmortem.
#[tokio::test]
async fn a_signal_killed_agent_reports_137_and_names_the_signal() {
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());
    let agent = fixture("oom-killed-agent.sh");

    let mut sup = SupervisorProc::spawn(&binary, agent.to_str().unwrap(), tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: TaskCommand::Scout(ScoutCommand::Start {
            task_id: "task_oom".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            prompt: "n/a".into(),
        }),
    })
    .await;

    let mut exit_code = None;
    let mut failure = None;
    while failure.is_none() {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Scout(ScoutEvent::ImplementationFinished { exit_code: code }),
            } => exit_code = Some(code),
            VmEvent::App {
                payload: TaskEvent::Scout(ScoutEvent::Failed { reason }),
            } => failure = Some(reason),
            VmEvent::App { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    assert_eq!(exit_code, Some(137), "SIGKILL should surface as 128 + 9");
    let reason = failure.unwrap();
    assert!(
        reason.contains("killed by signal 9 (SIGKILL)"),
        "reason did not name the signal: {reason}"
    );
    assert!(reason.contains("SPEC.md"), "reason: {reason}");

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

#[tokio::test]
async fn start_scout_fails_on_clone_error() {
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: TaskCommand::Scout(ScoutCommand::Start {
            task_id: "task_bad".into(),
            repo_clone_url: "file:///nonexistent/repo".into(),
            base_branch: "main".into(),
            prompt: "n/a".into(),
        }),
    })
    .await;

    // First event should be a Failed (clone error) — we should not see Started.
    match sup.recv().await {
        VmEvent::App {
            payload: TaskEvent::Scout(ScoutEvent::Failed { reason }),
        } => {
            assert!(reason.contains("clone"), "reason: {reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

/// The wire barrier's supervisor half: a Build command sent to a Scout VM is
/// refused with a terminal Failed, never acted on.
#[tokio::test]
async fn a_build_command_is_refused_not_acted_on() {
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();

    let mut sup = SupervisorProc::spawn(&binary, "true", tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));

    sup.send(VmCommand::App {
        payload: TaskCommand::Build(BuildCommand::Start {
            build_id: "build_x".into(),
            repo_clone_url: "file:///nowhere".into(),
            base_branch: "main".into(),
            branch: "build/build_x".into(),
            prompt: "n/a".into(),
        }),
    })
    .await;

    match sup.recv().await {
        VmEvent::App {
            payload: TaskEvent::Scout(ScoutEvent::Failed { reason }),
        } => assert!(reason.contains("scout"), "reason: {reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}
