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
            branch: BRANCH.into(),
            prompt: "## Spec 1 of 1: do the thing".into(),
        }),
    }
}

/// Drain until a terminal event, asserting the lifecycle shape on the way.
async fn drain(sup: &mut SupervisorProc) -> BuildEvent {
    drain_with_progress(sup).await.0
}

/// [`drain`], keeping the `Progress` lines — the transcript (#825) is where a
/// reviewer reads what the supervisor did to the branch.
async fn drain_with_progress(sup: &mut SupervisorProc) -> (BuildEvent, Vec<String>) {
    let mut progress = Vec::new();
    loop {
        match sup.recv().await {
            VmEvent::App {
                payload: TaskEvent::Build(evt),
            } => match evt {
                BuildEvent::Progress { line, .. } => progress.push(line),
                BuildEvent::Started { .. } | BuildEvent::ImplementationFinished { .. } => continue,
                terminal @ (BuildEvent::Completed { .. } | BuildEvent::Failed { .. }) => {
                    return (terminal, progress);
                }
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

const BRANCH: &str = "build/build_1";
const BRANCH_REF: &str = "refs/heads/build/build_1";
const ABANDONED_REF: &str = "refs/abandoned/build/build_1";

/// A finished build, landed the way the server lands it.
struct Landed {
    head_sha: String,
    files: Vec<String>,
    summary: Option<String>,
    progress: Vec<String>,
    /// Whether the bundle also carried `refs/abandoned/<branch>`. The server
    /// never fetches it — nothing may push it — but nothing may lose it either.
    abandoned: bool,
    scratch: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Landed {
    async fn tree(&self, r#ref: &str) -> String {
        run_git(&self.scratch, &["ls-tree", "-r", "--name-only", r#ref]).await
    }

    async fn blob(&self, r#ref: &str, path: &str) -> String {
        run_git(&self.scratch, &["show", &format!("{}:{path}", r#ref)]).await
    }

    fn said(&self, needle: &str) -> bool {
        self.progress.iter().any(|l| l.contains(needle))
    }
}

/// Run one build to `Completed` and unbundle it into a scratch repo **exactly
/// as the server will** — fetch the base from the remote, fetch the branch out
/// of the bundle by name, and run the server's tip check right here. A bundle
/// that agreed with the reported head by shipping the wrong ref would sail
/// through that comparison, so every caller then asserts on the contents.
async fn land(agent: &Path) -> Landed {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path()).await;
    let repo_url = format!("file://{}", repo.display());

    let mut sup =
        SupervisorProc::spawn(&supervisor_bin(), agent.to_str().unwrap(), tmp.path()).await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start(repo_url.clone())).await;

    let (terminal, progress) = drain_with_progress(&mut sup).await;
    let (base_sha, head_sha, bundle_base64, summary, files) = match terminal {
        BuildEvent::Completed {
            base_sha,
            head_sha,
            bundle_base64,
            summary,
            files_touched,
        } => (base_sha, head_sha, bundle_base64, summary, files_touched),
        other => panic!("expected Completed, got {other:?}"),
    };
    sup.send(VmCommand::Shutdown).await;
    sup.close().await;

    let scratch = tmp.path().join("scratch.git");
    tokio::fs::create_dir_all(&scratch).await.unwrap();
    run_git(&scratch, &["init", "--bare"]).await;
    run_git(&scratch, &["fetch", &repo_url, "main:refs/heads/main"]).await;
    let bundle_path = tmp.path().join("egress.bundle");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(bundle_base64)
        .expect("valid base64");
    tokio::fs::write(&bundle_path, bytes).await.unwrap();
    let bundle_arg = bundle_path.to_str().unwrap().to_string();
    run_git(
        &scratch,
        &["fetch", &bundle_arg, &format!("{BRANCH_REF}:{BRANCH_REF}")],
    )
    .await;

    // #891 itself: the check that discarded a finished build.
    let tip = run_git(&scratch, &["rev-parse", BRANCH_REF]).await;
    assert_eq!(tip, head_sha, "the server would reject this bundle");
    assert_ne!(head_sha, base_sha, "the branch grew");

    // The abandoned tip, if one rode along. Fetching one ref out of a two-ref
    // bundle works, which is also why `git push <branch_ref>` can never carry
    // the other one.
    let abandoned = git_ok(
        &scratch,
        &[
            "fetch",
            &bundle_arg,
            &format!("{ABANDONED_REF}:{ABANDONED_REF}"),
        ],
    )
    .await;

    Landed {
        head_sha,
        files,
        summary,
        progress,
        abandoned,
        scratch,
        _tmp: tmp,
    }
}

async fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .unwrap()
        .status
        .success()
}

/// #891 as reported: an agent that tidies its history from a detached HEAD.
///
/// The build branch stops tracking the work at the detach, so the supervisor
/// reported one commit (HEAD) and bundled another (the branch), and the server
/// threw the whole implementation away. The reconciliation takes HEAD, the
/// reported head is read back out of the bundle, and the old tip is kept.
#[tokio::test]
async fn a_rewritten_history_ships_where_the_agent_finished() {
    let landed = land(&fixture("history-rewriting-agent.sh")).await;

    let tree = landed.tree(BRANCH_REF).await;
    for file in ["src/one.rs", "src/two.rs", "src/three.rs"] {
        assert!(
            tree.contains(file),
            "{file} missing from the branch:\n{tree}"
        );
    }
    assert!(
        landed.files.contains(&"src/three.rs".to_string()),
        "files_touched is computed over base..head: {:?}",
        landed.files
    );
    assert!(
        landed.said("have diverged"),
        "the reconciliation is not in the transcript: {:?}",
        landed.progress
    );
    assert!(
        landed
            .summary
            .as_deref()
            .expect("summary")
            .contains("coherent series"),
        "SUMMARY.md is read before the artifacts are removed"
    );

    // The tip that was decided against is still in the bundle: no arm of the
    // reconciliation may lose a commit.
    assert!(landed.abandoned, "the pre-rewrite tip was not kept");
    assert!(landed.tree(ABANDONED_REF).await.contains("src/one.rs"));
    assert_ne!(
        run_git(&landed.scratch, &["rev-parse", ABANDONED_REF]).await,
        landed.head_sha,
        "the abandoned tip is the stale ref, not the commit that ships"
    );
}

/// The work is on the branch and HEAD is a stale checkout of the base — the
/// case where deriving the head from the bundle *alone* would agree on the
/// wrong tip and push a truncated branch with no complaint at all.
#[tokio::test]
async fn a_stranded_head_ships_the_branch_it_left_behind() {
    let landed = land(&fixture("stranded-head-agent.sh")).await;

    let tree = landed.tree(BRANCH_REF).await;
    assert!(
        tree.contains("src/implementation.rs"),
        "the implementation is missing:\n{tree}"
    );
    assert!(!tree.contains("PROMPT.md") && !tree.contains("SUMMARY.md"));
    assert!(
        landed.said("stale checkout"),
        "reconciliation missing: {:?}",
        landed.progress
    );
}

/// The reviewer's case, and the one that goes wrong if the sweep runs first:
/// work on the branch, HEAD detached on the base, and one file written *after*
/// the detach. Sweep-first commits that file onto the base, the two tips then
/// stand in no ancestor relation, the divergence arm prefers HEAD, and the PR
/// contains the sweep and none of the implementation.
#[tokio::test]
async fn a_dirty_stranded_head_keeps_the_branch_and_the_scratch_apart() {
    let landed = land(&fixture("stranded-dirty-head-agent.sh")).await;

    let tree = landed.tree(BRANCH_REF).await;
    assert!(
        tree.contains("src/implementation.rs"),
        "the implementation is missing from the branch:\n{tree}"
    );
    assert!(
        !tree.contains("src/scratch.rs"),
        "work from the abandoned checkout leaked onto the branch:\n{tree}"
    );

    assert!(landed.abandoned, "the stranded work was dropped");
    let kept = landed.tree(ABANDONED_REF).await;
    assert!(
        kept.contains("src/scratch.rs"),
        "the stranded file is nowhere:\n{kept}"
    );
    assert!(
        landed.said("left uncommitted off the build branch"),
        "the rescue is not in the transcript: {:?}",
        landed.progress
    );
}

/// A rebase left in progress: git parks HEAD on a partial replay while the
/// branch still holds the complete pre-rebase history, which ancestry cannot
/// tell apart from an ordinary divergence.
#[tokio::test]
async fn an_abandoned_rebase_ships_the_complete_pre_rebase_history() {
    let landed = land(&fixture("abandoned-rebase-agent.sh")).await;

    let tree = landed.tree(BRANCH_REF).await;
    assert!(
        tree.contains("src/implementation.rs"),
        "the implementation is missing:\n{tree}"
    );
    // The branch's own version of the conflicted file, not a replay of it.
    assert_eq!(landed.blob(BRANCH_REF, "src/conflict.rs").await, "ours");
    assert!(
        landed.said("a rebase is in progress"),
        "the rebase guard is not in the transcript: {:?}",
        landed.progress
    );
}

/// `git checkout --orphan` leaves HEAD unborn, so `git rev-parse HEAD` fails
/// outright — which used to be a fatal error *before* there was a bundle, the
/// one shape where the implementation is unrecoverable.
#[tokio::test]
async fn an_unborn_head_still_ships_the_branch() {
    let landed = land(&fixture("orphan-head-agent.sh")).await;

    let tree = landed.tree(BRANCH_REF).await;
    assert!(
        tree.contains("src/implementation.rs"),
        "the implementation is missing:\n{tree}"
    );
    assert!(!tree.contains("PROMPT.md") && !tree.contains("SUMMARY.md"));
    assert!(
        landed.said("HEAD is unborn"),
        "reconciliation missing: {:?}",
        landed.progress
    );
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
