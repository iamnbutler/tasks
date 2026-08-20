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
use tasks_protocol::verify::{VERIFY_SCRIPT_PATH, Verification, VerificationStatus};
use tasks_protocol::{
    BuildCommand, BuildEvent, FailureClass, ScoutCommand, TaskCommand, TaskEvent, TasksProtocol,
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
    make_repo(dir, None).await
}

/// A fixture repo, optionally **declaring a test suite in its base commit**.
///
/// The base commit is the point: the supervisor reads `.tasks/verify` at
/// `base_sha`, so a script committed anywhere else is not the one that runs —
/// which is the property `a_branch_that_weakens_its_own_gate_is_still_judged_by_the_base_one`
/// exists to prove.
async fn make_repo(dir: &Path, verify: Option<&str>) -> PathBuf {
    let repo = dir.join("fixture-repo");
    tokio::fs::create_dir_all(&repo).await.unwrap();
    run_git(&repo, &["init", "-b", "main"]).await;
    run_git(&repo, &["config", "user.email", "test@example.com"]).await;
    run_git(&repo, &["config", "user.name", "Test"]).await;
    tokio::fs::write(repo.join("README.md"), "# fixture\n")
        .await
        .unwrap();
    if let Some(script) = verify {
        let path = repo.join(VERIFY_SCRIPT_PATH);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, script).await.unwrap();
    }
    run_git(&repo, &["add", "-A"]).await;
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
    start_with_budget(repo_url, Some(600))
}

fn start_with_trunk(repo_url: String, trunk: Option<&str>) -> TVmCommand {
    let mut cmd = start_with_budget(repo_url, Some(600));
    if let VmCommand::App {
        payload: TaskCommand::Build(BuildCommand::Start { trunk_branch, .. }),
    } = &mut cmd
    {
        *trunk_branch = trunk.map(str::to_string);
    }
    cmd
}

/// A build **stacked** on another build's branch, which is the routine case
/// here and the one the trunk comparison exists for.
fn start_stacked(repo_url: String, base: &str) -> TVmCommand {
    let mut cmd = start_with_budget(repo_url, Some(600));
    if let VmCommand::App {
        payload: TaskCommand::Build(BuildCommand::Start { base_branch, .. }),
    } = &mut cmd
    {
        *base_branch = base.to_string();
    }
    cmd
}

fn start_with_budget(repo_url: String, budget_secs: Option<u64>) -> TVmCommand {
    VmCommand::App {
        payload: TaskCommand::Build(BuildCommand::Start {
            build_id: "build_1".into(),
            repo_clone_url: repo_url,
            base_branch: "main".into(),
            branch: BRANCH.into(),
            prompt: "## Spec 1 of 1: do the thing".into(),
            budget_secs,
            trunk_branch: Some("main".into()),
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
    /// What the supervisor's own run of the project's suite said. `None` only
    /// from a supervisor that predates the field, which this tree's cannot be.
    verification: Option<Verification>,
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
    land_verifying(agent, None, &[]).await
}

/// [`land`], with the repository declaring a test suite the supervisor will
/// run, and extra environment for the supervisor process.
async fn land_verifying(agent: &Path, verify: Option<&str>, env: &[(&str, &str)]) -> Landed {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_repo(tmp.path(), verify).await;
    let repo_url = format!("file://{}", repo.display());

    let mut sup =
        SupervisorProc::spawn_with_env(&supervisor_bin(), agent.to_str().unwrap(), tmp.path(), env)
            .await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start(repo_url.clone())).await;

    let (terminal, progress) = drain_with_progress(&mut sup).await;
    let (base_sha, head_sha, bundle_base64, summary, files, verification) = match terminal {
        BuildEvent::Completed {
            base_sha,
            head_sha,
            bundle_base64,
            summary,
            files_touched,
            verification,
        } => (
            base_sha,
            head_sha,
            bundle_base64,
            summary,
            files_touched,
            verification,
        ),
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
        verification,
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

    let (base_sha, head_sha, bundle_base64, summary, files, verification) =
        match drain(&mut sup).await {
            BuildEvent::Completed {
                base_sha,
                head_sha,
                bundle_base64,
                summary,
                files_touched,
                verification,
            } => (
                base_sha,
                head_sha,
                bundle_base64,
                summary,
                files_touched,
                verification,
            ),
            other => panic!("expected Completed, got {other:?}"),
        };

    // A repo that declares nothing is never green, and never a failure either:
    // refusing to dispatch would wedge a project on a convention it has not
    // adopted, which is exactly today's behaviour preserved.
    let verification = verification.expect("this supervisor always stamps a verification");
    assert_eq!(verification.status, VerificationStatus::Undeclared);
    assert!(!verification.is_green());

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
        BuildEvent::Failed { reason, class } => {
            assert!(reason.contains("no commits"), "reason: {reason}");
            // A build that committed nothing judged the work, and burns one
            // of its three. Waiving this would be switching the cap off.
            assert_eq!(class, FailureClass::Verdict);
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
                payload: TaskEvent::Build(BuildEvent::Failed { reason, class }),
            } => {
                // An OOM kill is deliberately still charged — see #828.
                assert_eq!(class, FailureClass::Verdict, "reason: {reason}");
                break reason;
            }
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
            payload: TaskEvent::Build(BuildEvent::Failed { reason, class }),
        } => {
            assert!(reason.contains("clone"), "reason: {reason}");
            assert_eq!(class, FailureClass::Verdict);
        }
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
            payload: TaskEvent::Build(BuildEvent::Failed { reason, class }),
        } => {
            assert!(reason.contains("builder"), "reason: {reason}");
            assert_eq!(class, FailureClass::Verdict);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
}

// --- the supervisor runs the project's suite (#1020) ---------------------
//
// The gate a project declares, as these tests declare it. Reading it out of the
// base commit is what makes the forgery in `gate-editing-agent.sh` fail.

/// Passes only once the sweep has already committed the work the agent left
/// behind — which is what pins the ordering the module doc claims: the suite
/// judges the tree the bundle carries, not the tree the agent walked away from.
const GATE_NEEDS_THE_SWEEP: &str = "#!/bin/sh\nexec git cat-file -e HEAD:src/forgotten.rs\n";

/// Red while `BROKEN` is in the tree; hangs forever while `SLOW` is.
const GATE_BROKEN_OR_SLOW: &str = "#!/bin/sh\n\
    if [ -f SLOW ]; then while :; do :; done; fi\n\
    test ! -f BROKEN\n";

/// Always red, whatever the branch does to its own copy of this file.
const GATE_ALWAYS_RED: &str = "#!/bin/sh\nexit 1\n";

/// Run a build with a declared gate and a stateful agent, returning the
/// terminal event and every progress line.
async fn build_with_gate(
    agent: &Path,
    gate: &str,
    env: &[(&str, &str)],
) -> (BuildEvent, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_repo(tmp.path(), Some(gate)).await;
    let repo_url = format!("file://{}", repo.display());
    let state = tmp.path().join("stub-state");
    tokio::fs::create_dir_all(&state).await.unwrap();

    let mut env: Vec<(&str, &str)> = env.to_vec();
    let state_str = state.to_str().unwrap().to_string();
    env.push(("STUB_STATE", &state_str));

    let mut sup = SupervisorProc::spawn_with_env(
        &supervisor_bin(),
        agent.to_str().unwrap(),
        tmp.path(),
        &env,
    )
    .await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start(repo_url)).await;
    let out = drain_with_progress(&mut sup).await;
    sup.send(VmCommand::Shutdown).await;
    sup.close().await;
    out
}

fn verification_of(event: &BuildEvent) -> &Verification {
    match event {
        BuildEvent::Completed { verification, .. } => {
            verification.as_ref().expect("a verification was stamped")
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// The happy path, and the ordering claim underneath it.
///
/// The gate passes only if the **swept** commit is already HEAD, so a green run
/// here is proof the suite judged the tree the bundle carries rather than
/// whatever the agent happened to leave on disk. Everything the build normally
/// ships still ships.
#[tokio::test]
async fn a_declared_suite_runs_against_the_swept_tree_and_its_pass_is_stamped() {
    let landed = land_verifying(&fixture_agent(), Some(GATE_NEEDS_THE_SWEEP), &[]).await;
    let v = landed.verification.as_ref().expect("verification stamped");
    assert_eq!(v.status, VerificationStatus::Passed);
    assert!(v.is_green());
    // The gate that ruled is named, always — a field that appears only on
    // disagreement is one nobody learns to read.
    assert!(v.detail.contains("gate "), "{}", v.detail);
    assert!(v.detail.contains("same as main"), "{}", v.detail);
    // And the build is otherwise exactly the build it was.
    assert!(landed.files.contains(&"src/built.rs".to_string()));
    assert!(landed.files.contains(&"src/forgotten.rs".to_string()));
    assert!(landed.said("verification: running .tasks/verify"));
}

/// An empty script is the cheapest possible forgery — `sh` on an empty file
/// exits 0 — and it must never read as a pass.
#[tokio::test]
async fn an_empty_declaration_is_undeclared_and_never_a_pass() {
    let landed = land_verifying(&fixture_agent(), Some(""), &[]).await;
    let v = landed.verification.as_ref().expect("verification stamped");
    assert_eq!(v.status, VerificationStatus::Undeclared);
    assert!(!v.is_green());
    assert!(v.detail.contains("empty"), "{}", v.detail);
    // It still ships: a project that declares nothing usable dispatches ungated,
    // which is exactly the behaviour that shipped before this check existed.
    assert!(!landed.head_sha.is_empty());
}

/// A red suite buys one repair round, on the same conversation and the same
/// worktree — and a build that goes green in it ships as a pass.
#[tokio::test]
async fn a_red_suite_gets_one_repair_round_and_ships_when_it_goes_green() {
    let (terminal, progress) = build_with_gate(
        &fixture("red-then-green-agent.sh"),
        GATE_BROKEN_OR_SLOW,
        &[],
    )
    .await;
    let v = verification_of(&terminal);
    assert_eq!(v.status, VerificationStatus::Passed, "{}", v.detail);
    assert!(
        progress.iter().any(|l| l.contains("one repair round")),
        "the repair round is announced in the transcript: {progress:?}"
    );
    // Read once per ROUND: the repair round's summary is the one that ships, or
    // the accounting the repair prompt asked for is dropped on the floor.
    match &terminal {
        BuildEvent::Completed { summary, .. } => assert!(
            summary
                .as_deref()
                .is_some_and(|s| s.contains("Fixed the failing test")),
            "the latest summary wins: {summary:?}"
        ),
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// A suite that is still red after its one repair round fails the build — and
/// packages nothing, which is the whole point: untested-and-broken work never
/// becomes a pull request for someone to review.
#[tokio::test]
async fn a_suite_that_stays_red_fails_the_build_and_packages_nothing() {
    let (terminal, _progress) = build_with_gate(
        &fixture("red-then-green-agent.sh"),
        GATE_BROKEN_OR_SLOW,
        &[("STUB_NEVER_FIX", "1")],
    )
    .await;
    match terminal {
        BuildEvent::Failed { reason, class } => {
            assert!(reason.contains("test suite failed"), "{reason}");
            assert!(reason.contains("after a repair round"), "{reason}");
            // A verdict on the work: the agent ran to completion twice and the
            // project's own suite says the result does not work.
            assert_eq!(class, FailureClass::Verdict);
        }
        other => panic!("a red suite must not package a bundle, got {other:?}"),
    }
}

/// **A red verdict, once reached, is not erased by an inconclusive re-run.**
///
/// Round one is red. The repair round trades the failure for a suite that never
/// finishes, which on a *first* round would be an honest `TimedOut` that ships.
/// Here it must not: the last thing actually known about this work is that it
/// failed, and shipping on "we do not know" after that is the one direction the
/// check exists to make impossible. It also closes the incentive gradient —
/// red is terminal, so a suite that merely hangs must not be an escape.
#[tokio::test]
async fn a_red_verdict_is_not_erased_by_an_inconclusive_re_run() {
    let (terminal, progress) = build_with_gate(
        &fixture("red-then-green-agent.sh"),
        GATE_BROKEN_OR_SLOW,
        &[("STUB_MAKE_SLOW", "1"), ("BUILDER_SUITE_BUDGET_SECS", "3")],
    )
    .await;
    match terminal {
        BuildEvent::Failed { reason, class } => {
            assert!(reason.contains("test suite failed"), "{reason}");
            assert!(reason.contains("did not fix it"), "{reason}");
            // The observed status stays HONEST — the second run really did time
            // out, and `detail` is read by humans. What changes is the decision,
            // not the observation.
            assert!(reason.contains("timed_out"), "{reason}");
            assert!(reason.contains("does not overturn a red run"), "{reason}");
            assert_eq!(class, FailureClass::Verdict);
        }
        other => panic!("an inconclusive re-run must not ship a red build, got {other:?}"),
    }
    // The killed run is reported rather than inferred from a silence.
    assert!(
        progress.iter().any(|l| l.contains("was killed")),
        "{progress:?}"
    );
}

/// A first-round timeout is different, and ships.
///
/// A suite that never finished is not evidence about the work — the
/// implementation may be perfect, and throwing it away because a cold `target/`
/// compiled slowly is the failure #929 and #884 were filed about. It is never
/// green, so the batch routes to a human.
///
/// This is also the test that found the hang: killing `sh` does not close the
/// pipes its children inherited, so awaiting the output collector here waits
/// forever. The gate uses a shell **builtin** loop rather than `sleep`, or the
/// leaked `sleep` adds its whole duration to every run of this suite.
#[tokio::test]
async fn a_first_round_timeout_ships_and_is_never_green() {
    let landed = land_verifying(
        &fixture_agent(),
        Some("#!/bin/sh\nwhile :; do :; done\n"),
        &[("BUILDER_SUITE_BUDGET_SECS", "3")],
    )
    .await;
    let v = landed.verification.as_ref().expect("verification stamped");
    assert_eq!(v.status, VerificationStatus::TimedOut);
    assert!(!v.is_green());
    assert!(v.detail.contains("killed after 3s"), "{}", v.detail);
    // It shipped: the bundle is real and the branch grew.
    assert!(landed.files.contains(&"src/built.rs".to_string()));
}

/// The forgery this whole change exists to prevent, one level down.
///
/// The agent rewrites `.tasks/verify` to `exit 0` and commits it. The build
/// still fails, because the script is read out of the **base** commit and the
/// branch's version is never the one that runs.
#[tokio::test]
async fn a_branch_that_weakens_its_own_gate_is_still_judged_by_the_base_one() {
    let (terminal, progress) =
        build_with_gate(&fixture("gate-editing-agent.sh"), GATE_ALWAYS_RED, &[]).await;
    match terminal {
        BuildEvent::Failed { reason, class } => {
            assert!(reason.contains("test suite failed"), "{reason}");
            assert_eq!(class, FailureClass::Verdict);
        }
        other => panic!("a weakened gate must not let a red build ship, got {other:?}"),
    }
    // The gate that ruled is the trunk's, and the branch's edit changed nothing
    // about it — so there is deliberately NO divergence to report here. The
    // comparison is base-against-trunk, and on an unstacked build those are the
    // same commit; the case it exists for is the one below.
    assert!(
        progress.iter().any(|l| l.contains("same as main")),
        "{progress:?}"
    );
    assert!(
        !progress.iter().any(|l| l.contains("declaration_changed")),
        "an unstacked build has no divergence to report: {progress:?}"
    );
}

/// **Stacking punctures the base-commit defence, and the comparison that
/// notices is against the trunk.**
///
/// Build A weakens `.tasks/verify` and opens a pull request. Build B is
/// dispatched onto A's branch, so the weakened script is *already in B's base
/// commit*: B's own diff changes nothing, and a base-against-own-diff
/// comparison sees nothing wrong. This pipeline stacks builds as a matter of
/// course, so that is not a corner case.
///
/// The gate still rules — it is reported, never refused, because changing how a
/// project is tested is ordinary work — but which script ruled is said out
/// loud, so a reviewer can see the run was gated by something the trunk does
/// not have.
#[tokio::test]
async fn a_gate_weakened_by_an_earlier_build_in_the_stack_is_reported_against_the_trunk() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_repo(tmp.path(), Some(GATE_ALWAYS_RED)).await;

    // Build A's branch: same repo, a gate that passes anything.
    run_git(&repo, &["checkout", "-q", "-b", "build/earlier"]).await;
    tokio::fs::write(
        repo.join(VERIFY_SCRIPT_PATH),
        "#!/bin/sh
exit 0
",
    )
    .await
    .unwrap();
    run_git(&repo, &["add", "-A"]).await;
    run_git(&repo, &["commit", "-q", "-m", "Relax the gate"]).await;
    run_git(&repo, &["checkout", "-q", "main"]).await;

    let repo_url = format!("file://{}", repo.display());
    let mut sup = SupervisorProc::spawn(
        &supervisor_bin(),
        fixture_agent().to_str().unwrap(),
        tmp.path(),
    )
    .await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start_stacked(repo_url, "build/earlier")).await;
    let (terminal, progress) = drain_with_progress(&mut sup).await;
    sup.send(VmCommand::Shutdown).await;
    sup.close().await;

    // The weakened gate really did rule — that is the cost of reporting rather
    // than refusing, and it is the cost the reviewer chose.
    let v = verification_of(&terminal);
    assert_eq!(v.status, VerificationStatus::Passed);
    // But it is not passed off as the trunk's, and the difference is named on
    // the field itself rather than only in a log line nobody reads back.
    assert!(v.detail.contains("DIFFERS from main"), "{}", v.detail);
    assert!(
        progress.iter().any(|l| l.contains("declaration_changed")),
        "the divergence must be reported: {progress:?}"
    );
}

/// The trunk comparison is best-effort, and an unmade comparison says so rather
/// than reading as agreement.
#[tokio::test]
async fn an_unreachable_trunk_is_reported_as_an_unmade_comparison() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_repo(tmp.path(), Some(GATE_NEEDS_THE_SWEEP)).await;
    let repo_url = format!("file://{}", repo.display());

    let mut sup = SupervisorProc::spawn(
        &supervisor_bin(),
        fixture_agent().to_str().unwrap(),
        tmp.path(),
    )
    .await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start_with_trunk(repo_url, Some("no-such-trunk")))
        .await;
    let (terminal, _progress) = drain_with_progress(&mut sup).await;
    sup.send(VmCommand::Shutdown).await;
    sup.close().await;

    let v = verification_of(&terminal);
    // Still green — an unmade comparison is not a reason to refuse a passing
    // run — but the gate identity says the comparison was not made.
    assert_eq!(v.status, VerificationStatus::Passed);
    assert!(v.detail.contains("gate "), "{}", v.detail);
    assert!(
        v.detail.contains("not reachable in this clone"),
        "an unmade comparison must not read as agreement: {}",
        v.detail
    );
}

/// A run with no budget left to state has nothing to bound a suite with, and
/// says so — `Unavailable`, never a guess, and never green.
#[tokio::test]
async fn a_host_that_states_no_budget_reports_unavailable_rather_than_guessing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_repo(tmp.path(), Some(GATE_NEEDS_THE_SWEEP)).await;
    let repo_url = format!("file://{}", repo.display());

    let mut sup = SupervisorProc::spawn(
        &supervisor_bin(),
        fixture_agent().to_str().unwrap(),
        tmp.path(),
    )
    .await;
    assert!(matches!(sup.recv().await, VmEvent::Ready));
    sup.send(start_with_budget(repo_url, None)).await;
    let (terminal, _progress) = drain_with_progress(&mut sup).await;
    sup.send(VmCommand::Shutdown).await;
    sup.close().await;

    let v = verification_of(&terminal);
    assert_eq!(v.status, VerificationStatus::Unavailable);
    assert!(!v.is_green());
    assert!(v.detail.contains("restart it"), "{}", v.detail);
}
