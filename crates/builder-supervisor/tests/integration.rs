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

/// Path to the builder-supervisor binary. Cargo builds it for us as a
/// dependency of this test target and hands over the path — tests exec
/// binaries, they never build them.
fn supervisor_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_builder-supervisor"))
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

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture_agent() -> PathBuf {
    fixture("stub-builder-agent.sh")
}

struct SupervisorProc {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl SupervisorProc {
    async fn spawn(binary: &Path, agent_cmd: &str, workdir_root: &Path) -> Self {
        Self::spawn_with_env(binary, agent_cmd, workdir_root, &[]).await
    }

    async fn spawn_with_env(
        binary: &Path,
        agent_cmd: &str,
        workdir_root: &Path,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let mut cmd = Command::new(binary);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("BUILDER_AGENT_CMD", agent_cmd)
            .env("BUILDER_WORKDIR_ROOT", workdir_root);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
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
    let binary = supervisor_bin();
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
    let binary = supervisor_bin();
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

/// The OOM shape: the agent is killed by a signal, commits nothing, and until
/// #825 lands the failure reason is the only place a build's postmortem can
/// live. So the reason must name the signal, and the exit code must be
/// `128 + 9` rather than the `-1` every other odd failure used to share.
#[tokio::test]
async fn a_signal_killed_agent_reports_137_and_names_the_signal() {
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());

    let agent = fixture("oom-killed-agent.sh");
    let mut sup = SupervisorProc::spawn(&binary, agent.to_str().unwrap(), tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start(repo_url)).await;

    let mut exit_code = None;
    let reason = loop {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Build(BuildEvent::ImplementationFinished { exit_code: code }),
            } => exit_code = Some(code),
            VmEvent::App {
                payload: TaskEvent::Build(BuildEvent::Failed { reason }),
            } => break reason,
            VmEvent::App {
                payload: TaskEvent::Build(_),
            } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };

    assert_eq!(exit_code, Some(137), "SIGKILL should surface as 128 + 9");
    assert!(reason.contains("no commits"), "reason: {reason}");
    assert!(
        reason.contains("killed by signal 9 (SIGKILL)"),
        "reason did not name the signal: {reason}"
    );

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

/// #845 on the Builder side: the agent's API connection dies mid-response and
/// the build still ships.
///
/// The resume happens inside this VM, so the conversation *and* the worktree
/// survive — the commit the agent made before the drop is still there for it
/// to build on. A host-side retry would get a new VM and a fresh clone, and
/// for a Builder that worktree is the implementation. The bundle carrying both
/// halves is the proof.
#[tokio::test]
async fn a_dropped_api_connection_is_resumed_and_the_build_still_ships() {
    let binary = supervisor_bin();
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());
    let agent = fixture("stub-builder-agent-api-death.sh");
    let state = tmp.path().join("stub-state");
    std::fs::create_dir_all(&state).unwrap();

    // One resume: enough to prove the loop, and the 2s backoff is real.
    let mut sup = SupervisorProc::spawn_with_env(
        &binary,
        agent.to_str().unwrap(),
        tmp.path(),
        &[
            ("STUB_STATE", state.to_str().unwrap()),
            ("BUILDER_MAX_RESUMES", "1"),
        ],
    )
    .await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start(repo_url.clone())).await;

    let mut announced_resume = false;
    let mut exit_code = None;
    let terminal = loop {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Build(BuildEvent::Progress { line, .. }),
            } => {
                if line.contains("resuming session") {
                    announced_resume = true;
                }
            }
            VmEvent::App {
                payload: TaskEvent::Build(BuildEvent::ImplementationFinished { exit_code: code }),
            } => exit_code = Some(code),
            VmEvent::App {
                payload:
                    TaskEvent::Build(evt @ (BuildEvent::Completed { .. } | BuildEvent::Failed { .. })),
            } => break evt,
            VmEvent::App { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    };

    assert!(
        announced_resume,
        "the resume boundary must reach the build transcript as a Progress line"
    );
    // The LAST attempt's code, not the death's: this event describes the run.
    assert_eq!(exit_code, Some(0));

    let (head_sha, bundle_base64, summary, files) = match terminal {
        BuildEvent::Completed {
            head_sha,
            bundle_base64,
            summary,
            files_touched,
            ..
        } => (head_sha, bundle_base64, summary, files_touched),
        other => panic!("a resumed build should complete: {other:?}"),
    };

    // Both halves: the one committed before the connection died and the one
    // committed after. Losing the first is what a host-side retry would cost.
    assert!(
        files.contains(&"src/first_half.rs".to_string()),
        "{files:?}"
    );
    assert!(
        files.contains(&"src/second_half.rs".to_string()),
        "{files:?}"
    );
    assert!(summary.expect("summary").contains("resumed once"));

    // Unbundle exactly as the server will: the branch must carry both commits.
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
    let listing = run_git(&scratch, &["ls-tree", "-r", "--name-only", &head_sha]).await;
    assert!(listing.contains("src/first_half.rs"), "{listing}");
    assert!(listing.contains("src/second_half.rs"), "{listing}");

    // The fixture exits 9 if it is resumed into the wrong conversation or
    // finds its worktree missing, so two attempts and a clean build is the
    // proof that neither happened.
    let attempts = std::fs::read_to_string(state.join("attempts")).unwrap();
    assert_eq!(attempts.trim(), "2", "expected exactly one resume");

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

#[tokio::test]
async fn a_clone_error_fails_before_started() {
    let binary = supervisor_bin();
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
    let binary = supervisor_bin();
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
