//! builder-supervisor: PID 1 binary that runs inside a Builder VM.
//!
//! Protocol: speaks JSON-lines over stdin/stdout using vm-pool's
//! VmCommand/VmEvent envelopes with [`TasksProtocol`] payloads. Answers only
//! the `build` role; a `scout` command is refused with a terminal `Failed` —
//! that refusal is the supervisor half of the Scout/Builder barrier.
//!
//! On [`BuildCommand::Start`]:
//!   1. Creates a workdir
//!   2. Clones the repo at `base_branch` — at FULL depth: `git bundle` refuses
//!      to package history out of a shallow repository, and the branch (not a
//!      text artifact) is this supervisor's deliverable
//!   3. Records the base commit SHA, checks out the HOST-chosen `branch`
//!      (the server pushes it, so the server names it)
//!   4. Writes the prompt to `PROMPT.md` and pipes it to the agent's stdin
//!   5. Runs the configured agent command, streaming stdout/stderr as Progress,
//!      and resumes it in place (`--resume <session_id>`, up to
//!      `BUILDER_MAX_RESUMES`) if its API connection drops mid-response — the
//!      worktree survives with the conversation, which is the whole point for
//!      a Builder. See [`tasks_protocol::agent_run`]
//!   6. Reads `SUMMARY.md` (optional PR prose), then removes both artifacts
//!      from the worktree
//!   7. [`reconcile_checkout`]s the build branch with wherever the agent
//!      actually finished — HEAD is only the branch while it stays
//!      symbolically attached to it, and a rebase or a `git checkout <sha>`
//!      breaks that silently. **Before** the sweep, always: the sweep commits
//!      onto whatever HEAD is
//!   8. Sweeps everything the agent left uncommitted into a final commit —
//!      losing a build to a forgotten `git commit` is a bad failure mode
//!   9. `tip == base` → `Failed` (the analogue of a scout's missing SPEC.md)
//!  10. Runs the project's own test suite ([`run_verification`]) and stamps a
//!      [`Verification`] on the terminal event. A **red** suite buys one
//!      bounded repair round and then fails the build — it never packages a
//!      bundle, so untested-and-broken work cannot reach GitHub at all
//!  11. Emits `Completed` with a base64 thin bundle of `base_sha..branch`,
//!      whose `head_sha` is read back **out of the bundle** so there is one
//!      value where there used to be two that had to agree
//!
//! Steps 6-10 are a **loop**, because a repair round's commits have to travel
//! the same path: read the summary again (the repair prompt asks the agent to
//! account for changed tests in it), reconcile again, sweep again, and only
//! then re-run the suite. The suite runs *after* the reconciliation and the
//! sweep and immediately before the packaging, because the sweep is what turns
//! the working tree into the branch and the reconciliation is what decides
//! which tip the build *is* (#891) — a suite run any earlier judges a tree that
//! is not the deliverable.
//!
//! No credential ever needs to enter this VM for egress: the bundle rides the
//! event stream and the server pushes with its own token.
//!
//! The agent command is configured via `BUILDER_AGENT_CMD` (tokens
//! space-separated; no shell expansion). Default: `claude --print`.
//!
//! # Why the supervisor runs the suite and the agent does not
//!
//! What this replaced was `Verification: PASSED|FAILED|NOT RUN`, a line the
//! agent wrote into `SUMMARY.md` and the host grepped to decide whether a pull
//! request could be landed. That is a gate on prose authored by the party
//! being graded. Running it here makes it a check: the graded party cannot
//! write the answer, and — the larger prize — a red suite never becomes a pull
//! request at all, where before it opened one, parked a batch in
//! `awaiting_merge` and spent a reviewer's attention.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use tasks_protocol::agent_run::{
    AgentRun, RESUME_PROMPT, ResultWatcher, ResumeDecision, command_selects_session,
    max_resumes_from_env, resume_argv,
};
use tasks_protocol::redact::redact;
use tasks_protocol::verify::{
    SuiteBudget, VERIFY_SCRIPT_PATH, Verification, VerificationStatus, suite_budget_cap_from_env,
    suite_budget_secs,
};
use tasks_protocol::vm_memory::{AgentOutcome, MemorySample, sample_memory};
use tasks_protocol::{
    BuildCommand, BuildEvent, FailureClass, LogStream, MAX_BUNDLE_BASE64_BYTES, SupervisorBuild,
    TaskCommand, TaskEvent, TasksProtocol,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use vm_pool_protocol::{VmCommand, VmEvent};

type TaskVmCommand = VmCommand<TasksProtocol>;
type TaskVmEvent = VmEvent<TasksProtocol>;

/// This binary's build identity, stamped by `build-stamp` in `build.rs` —
/// the same crate and the same scheme the server uses, which is what makes
/// the two numbers comparable at all.
pub fn identity() -> SupervisorBuild {
    SupervisorBuild {
        version: env!("BUILDER_SUPERVISOR_VERSION").to_string(),
        commit: env!("BUILDER_SUPERVISOR_COMMIT").to_string(),
    }
}

/// Answer `--version` and say whether we did.
///
/// Called **before** the tracing setup and before stdin is touched, because
/// `make images-check` runs this by booting the image (`container run --rm
/// agent:v1 --version`) and reading one line off stdout — the supervisor is
/// the image's ENTRYPOINT, so argv reaches here.
///
/// One line, three whitespace-separated fields (`<name> <version> <commit>`),
/// so `awk '{print $2}'` is the whole parser. That shape is a contract with
/// the Makefile: change it and change `images-check` with it.
fn answered_version() -> bool {
    if !std::env::args().skip(1).any(|arg| arg == "--version") {
        return false;
    }
    let build = identity();
    println!("builder-supervisor {} {}", build.version, build.commit);
    true
}

#[tokio::main]
async fn main() -> Result<()> {
    // See scout-supervisor: answered before tracing and before stdin.
    if answered_version() {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();
    info!("builder-supervisor starting");

    let (evt_tx, mut evt_rx) = mpsc::channel::<TaskVmEvent>(128);

    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(event) = evt_rx.recv().await {
            if let Err(e) = write_event(&mut stdout, &event).await {
                error!("failed to write event: {e}");
                break;
            }
        }
    });

    evt_tx.send(VmEvent::Ready).await.ok();

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                info!("stdin closed, shutting down");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                error!("stdin read error: {e}");
                break;
            }
        }

        let command: TaskVmCommand = match serde_json::from_str(line.trim()) {
            Ok(c) => c,
            Err(e) => {
                // Redacted: the line we could not decode is a `Start`
                // carrying `repo_clone_url`, which holds `GITHUB_TOKEN` as
                // basic auth — and this process's stderr is inherited up
                // through `container run` into vm-pool's own log. The host it
                // names stays readable, which is the diagnostic half.
                warn!("invalid command line ({e}): {}", redact(line.trim()));
                continue;
            }
        };

        match command {
            VmCommand::Ping => {
                evt_tx.send(VmEvent::Pong).await.ok();
            }
            VmCommand::Shutdown => {
                info!("shutdown requested");
                evt_tx.send(VmEvent::Shutdown).await.ok();
                break;
            }
            VmCommand::App {
                payload:
                    TaskCommand::Build(BuildCommand::Start {
                        build_id,
                        repo_clone_url,
                        base_branch,
                        branch,
                        prompt,
                        budget_secs,
                        trunk_branch,
                    }),
            } => {
                let tx = evt_tx.clone();
                run_build(
                    &build_id,
                    &repo_clone_url,
                    &base_branch,
                    &branch,
                    &prompt,
                    budget_secs,
                    trunk_branch.as_deref(),
                    tx,
                )
                .await;
            }
            VmCommand::App {
                payload: TaskCommand::Scout(_),
            } => {
                warn!("received a scout command; this VM is a builder");
                emit(
                    &evt_tx,
                    BuildEvent::Failed {
                        reason: "this VM is a builder; refusing a scout command".into(),
                        class: FailureClass::Verdict,
                    },
                )
                .await;
            }
        }
    }

    drop(evt_tx);
    let _ = writer.await;
    info!("builder-supervisor exiting");
    Ok(())
}

async fn write_event(stdout: &mut tokio::io::Stdout, event: &TaskVmEvent) -> Result<()> {
    let json = serde_json::to_string(event)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

async fn emit(tx: &mpsc::Sender<TaskVmEvent>, event: BuildEvent) {
    let _ = tx
        .send(VmEvent::App {
            payload: TaskEvent::Build(event),
        })
        .await;
}

/// Run the Builder workflow. Every failure path emits `Failed` and returns.
#[allow(clippy::too_many_arguments)]
async fn run_build(
    build_id: &str,
    repo_clone_url: &str,
    base_branch: &str,
    branch: &str,
    prompt: &str,
    budget_secs: Option<u64>,
    trunk_branch: Option<&str>,
    tx: mpsc::Sender<TaskVmEvent>,
) {
    // Anchored before the clone, so the suite's budget is sized against what
    // is *left* rather than against what the run was given. Monotonic: a
    // suspended host is the outer deadline's business (the host holds it and
    // classifies it), and a suite budget that grew across a lid would be the
    // one thing that cannot help — the outer deadline kills the VM regardless.
    let started = Instant::now();
    // The `class:` arm must come FIRST: the catch-all below matches
    // `class: …` too, and would wrap a classified failure in a second class.
    //
    // Unclassified is [`FailureClass::Verdict`], which is what every
    // *pre-agent* site wants — a clone against a base branch that no longer
    // exists fails identically every time, so waiving it would mean a batch
    // retrying forever with nothing to stop it. Only the sites after the agent
    // has run have an [`AgentRun`] to ask, and they pass `run.failure_class()`.
    macro_rules! fail {
        (class: $class:expr, $($arg:tt)*) => {{
            emit(&tx, BuildEvent::Failed { reason: format!($($arg)*), class: $class }).await;
            return;
        }};
        ($($arg:tt)*) => {{
            emit(
                &tx,
                BuildEvent::Failed {
                    reason: format!($($arg)*),
                    class: FailureClass::Verdict,
                },
            )
            .await;
            return;
        }};
    }

    let workdir = match make_workdir(build_id) {
        Ok(w) => w,
        Err(e) => fail!("workdir: {e}"),
    };
    debug!(build_id, workdir = %workdir.display(), "build workdir created");

    // Full-depth clone — see the module doc. This is the single easiest way to
    // break branch egress while everything else still passes.
    if let Err(e) = git(
        &workdir,
        &["clone", "--branch", base_branch, repo_clone_url, "."],
    )
    .await
    {
        fail!("clone: {e}");
    }
    // Identity for the sweep commit; a bare clone has none configured.
    for args in [
        ["config", "user.email", "builder@tasks.invalid"].as_slice(),
        ["config", "user.name", "Tasks Builder"].as_slice(),
    ] {
        if let Err(e) = git(&workdir, args).await {
            fail!("git config: {e}");
        }
    }

    let base_sha = match git_stdout(&workdir, &["rev-parse", "HEAD"]).await {
        Ok(sha) => sha,
        Err(e) => fail!("rev-parse: {e}"),
    };

    if let Err(e) = git(&workdir, &["checkout", "-b", branch]).await {
        fail!("branch: {e}");
    }
    emit(
        &tx,
        BuildEvent::Started {
            base_sha: base_sha.clone(),
            supervisor: Some(identity()),
        },
    )
    .await;

    if let Err(e) = tokio::fs::write(workdir.join("PROMPT.md"), prompt).await {
        fail!("prompt write: {e}");
    }

    let run = match run_agent(&workdir, prompt, tx.clone()).await {
        Ok(run) => run,
        Err(e) => fail!("agent: {e}"),
    };
    // The exit code of the *last* attempt, so a build that was resumed across
    // a dropped connection and then finished cleanly reports 0. This event
    // describes the run, not the first process — see [`AgentRun`].
    emit(
        &tx,
        BuildEvent::ImplementationFinished {
            exit_code: run.outcome.exit_code,
        },
    )
    .await;

    // Steps 6-10, once per round. A red suite buys exactly one repair round,
    // and the round's commits have to travel this same path — which is why
    // this is a loop and not a straight line.
    let mut run = run;
    let mut summary: Option<String> = None;
    let mut abandoned: Vec<String> = Vec::new();
    // The first round that came back red, if one did. Once this is set, no
    // later status may package a bundle: we reached a verdict and then spent a
    // repair round failing to overturn it, and shipping on "we do not know"
    // when the last thing we actually knew was red is the one direction this
    // whole check exists to make impossible. It is kept separately from the
    // round's own observed status precisely so the second round's status stays
    // honest — a repair round that times out reports `TimedOut`, and the build
    // fails anyway.
    let mut first_red: Option<String> = None;
    let mut repair_spent = false;

    let (verification, tip) = loop {
        // Read once per ROUND, not once before the loop: the repair prompt asks
        // the agent to account for changed tests in the summary, and a pre-loop
        // read drops exactly that. Latest non-empty wins, so a repair round
        // that rewrote nothing keeps the first round's prose. Optional
        // throughout: missing prose does not fail a build — the code is the
        // deliverable.
        if let Some(latest) = read_summary(&workdir).await {
            summary = Some(latest);
        }

        // Remove the artifacts from the worktree, reconcile, then sweep. `git
        // add -A` stages the removals as deletions if the agent committed
        // them, alongside any implementation work the agent left uncommitted.
        remove_artifacts(&workdir).await;

        // The reconciliation runs BEFORE the sweep, and the order is
        // load-bearing: the sweep commits onto whatever HEAD is, so on a
        // stranded checkout a sweep-first ordering manufactures a divergence no
        // ancestry rule can undo — and the build ships a PR containing the
        // sweep and none of the implementation. See [`reconcile_checkout`].
        match reconcile_checkout(&workdir, branch, &tx).await {
            // Accumulated across rounds: the reconciliation runs once per
            // round, so a repair round can strand a second tip, and a single
            // `Option` could only hold one of them.
            Ok(Some(left)) => abandoned.push(left),
            Ok(None) => {}
            Err(e) => fail!(class: run.failure_class(), "reconcile: {e}"),
        }
        // A `checkout --force` back onto the branch can restore an artifact the
        // agent had committed there; removing them again turns that into a
        // deletion the sweep below picks up.
        remove_artifacts(&workdir).await;

        if let Err(e) = commit_worktree(&workdir, SWEEP_MESSAGE).await {
            fail!(class: run.failure_class(), "sweep: {e}");
        }

        let branch_ref = format!("refs/heads/{branch}");
        let tip = match git_stdout(&workdir, &["rev-parse", &branch_ref]).await {
            Ok(sha) => sha,
            Err(e) => fail!(class: run.failure_class(), "rev-parse branch: {e}"),
        };
        if tip == base_sha {
            // "no commits" on its own reads as a verdict on the agent's work,
            // so an OOM kill, a signal death or a dropped API connection (#845)
            // is named here rather than left for someone to infer from a budget
            // that vanished.
            fail!(
                class: run.failure_class(),
                "agent produced no commits (tip == base){}",
                run.failure_context()
            );
        }

        // The suite judges the swept tree, which is the thing the bundle will
        // carry — see the module docs for why nothing earlier would do.
        let remaining = budget_secs.map(|b| b.saturating_sub(started.elapsed().as_secs()));
        match run_verification(&workdir, &base_sha, trunk_branch, remaining, &tx).await {
            SuiteResult::Reported(verification) => match &first_red {
                // A verdict was reached, the repair round did not overturn it,
                // and whatever the re-run observed is not evidence that it did.
                // The observed status is reported in the reason rather than
                // rewritten into a red it never was.
                Some(red) if !verification.is_green() => fail!(
                    class: FailureClass::Verdict,
                    "the project's test suite failed and the repair round did not fix it: \
                     {red} — the re-run then reported {} ({}), which does not overturn a red \
                     run, so nothing was packaged",
                    verification.status,
                    or_unstated(&verification.detail),
                ),
                _ => break (verification, tip),
            },
            SuiteResult::Red { detail, tail } => {
                if repair_spent {
                    fail!(
                        class: FailureClass::Verdict,
                        "the project's test suite failed, and failed again after a repair \
                         round: {detail}",
                    );
                }
                let Some(session_id) = run.session_id.clone() else {
                    fail!(
                        class: FailureClass::Verdict,
                        "the project's test suite failed: {detail} — and the agent announced \
                         no session id, so there was no conversation to hand the failure back \
                         to",
                    );
                };
                repair_spent = true;
                first_red = Some(detail.clone());
                match repair_round(&workdir, &session_id, &detail, &tail, &tx).await {
                    Ok(repaired) => run = repaired,
                    Err(e) => fail!(
                        class: FailureClass::Verdict,
                        "the project's test suite failed: {detail} — and the repair round \
                         could not be started: {e}",
                    ),
                }
            }
        }
    };

    // Thin bundle with base_sha as its prerequisite, carrying the branch ref
    // the server will fetch by name — and the head is read back out of it.
    let (bundle_base64, head_sha) =
        match package_bundle(&workdir, &base_sha, branch, &abandoned, &tx).await {
            Ok(packaged) => packaged,
            Err(e) => fail!(class: run.failure_class(), "bundle: {e}"),
        };
    if head_sha != tip {
        // Not fatal: the bundle is the deliverable and `head_sha` now
        // describes it. Loud, because nothing should be able to cause this.
        progress(
            &tx,
            format!(
                "builder-supervisor: the bundle carries {branch} at {} but the worktree reads \
                 {}; shipping the bundle's, which is what the server receives",
                short(&head_sha),
                short(&tip)
            ),
        )
        .await;
    }

    let files_touched = match git_stdout(
        &workdir,
        &["diff", "--name-only", &format!("{base_sha}..{head_sha}")],
    )
    .await
    {
        Ok(out) => out.lines().map(str::to_string).collect(),
        Err(e) => {
            warn!("could not compute files_touched: {e}");
            Vec::new()
        }
    };

    emit(
        &tx,
        BuildEvent::Completed {
            base_sha,
            head_sha,
            bundle_base64,
            summary,
            files_touched,
            verification: Some(verification),
        },
    )
    .await;
}

/// `SUMMARY.md`, if the agent wrote one with anything in it.
async fn read_summary(workdir: &Path) -> Option<String> {
    tokio::fs::read_to_string(workdir.join("SUMMARY.md"))
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A detail string, or a stand-in when there is none. Never an empty pair of
/// brackets in a reason a human reads.
fn or_unstated(detail: &str) -> &str {
    match detail.trim().is_empty() {
        true => "no detail",
        false => detail,
    }
}

/// The sweep commit: work the agent left uncommitted *on* the build branch.
const SWEEP_MESSAGE: &str = "Sweep: work the agent left uncommitted";

/// The rescue commit: work the agent left uncommitted on a checkout the
/// reconciliation is about to leave. It is never pushed — it rides the bundle
/// as `refs/abandoned/<branch>`.
const STRANDED_MESSAGE: &str = "Stranded: work the agent left uncommitted off the build branch";

/// Both supervisor-written artifacts, out of the worktree. Best-effort: a
/// missing file is the ordinary case (the agent committed it, or never had
/// one), and the sweep turns a committed one into a deletion.
async fn remove_artifacts(workdir: &Path) {
    for artifact in ["PROMPT.md", "SUMMARY.md"] {
        let _ = tokio::fs::remove_file(workdir.join(artifact)).await;
    }
}

/// Decide which tip this build *is*, and leave HEAD attached to the build
/// branch at it.
///
/// The supervisor reports one commit and bundles another: `git rev-parse HEAD`
/// and `refs/heads/<branch>` are the same commit only while HEAD stays
/// symbolically attached to the host-chosen branch. A rebase, a `git checkout
/// <sha>` to look at something, or a branch of the agent's own detaches HEAD,
/// and from that moment the branch ref silently stops tracking the work. That
/// is #891, where a finished build was discarded for a mismatch it could not
/// have avoided — and, worse, where reading the head out of the bundle *alone*
/// would have shipped the stale tip with no complaint at all.
///
/// **This must run before the sweep.** The sweep commits onto whatever HEAD
/// is; on a stranded checkout that manufactures a divergence no ancestry rule
/// can undo, and the build opens a PR containing the sweep and none of the
/// implementation.
///
/// Returns the tip it decided *against*, when that tip is not reachable from
/// the one it chose. The caller packages it as `refs/abandoned/<branch>`, so no
/// arm of this can lose a commit.
async fn reconcile_checkout(
    workdir: &Path,
    branch: &str,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<Option<String>> {
    let branch_ref = format!("refs/heads/{branch}");
    let head = git_stdout(workdir, &["rev-parse", "HEAD"])
        .await
        .ok()
        .filter(|s| !s.is_empty());
    let attached = git_stdout(workdir, &["symbolic-ref", "--quiet", "HEAD"])
        .await
        .is_ok_and(|r| r == branch_ref);
    // `--verify --quiet`, so a ref that does not exist is None rather than the
    // string echoed back at us.
    let tip = git_stdout(
        workdir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{branch_ref}^{{commit}}"),
        ],
    )
    .await
    .ok()
    .filter(|s| !s.is_empty());

    // 1. Attached and equal — the overwhelmingly common path. Its whole cost
    //    is one extra rev-parse and one symbolic-ref.
    if attached
        && let (Some(head), Some(tip)) = (&head, &tip)
        && head == tip
    {
        return Ok(None);
    }

    // 2. HEAD unborn (`git checkout --orphan`). Nothing to compare, but the
    //    branch may hold a whole implementation — and this used to die on
    //    `rev-parse head:`, before there was a bundle for the server to keep.
    let Some(head) = head else {
        let tip = tip.context("HEAD is unborn and the build branch does not exist")?;
        reconciling(
            tx,
            format!(
                "HEAD is unborn (git checkout --orphan); taking {branch} at {}",
                short(&tip)
            ),
        )
        .await;
        return take_branch(workdir, branch, tx).await;
    };

    // 3. A rebase in progress is the trap in "prefer HEAD when diverged": git
    //    leaves HEAD on a partial replay and keeps the branch at the complete
    //    pre-rebase tip, which ancestry cannot tell apart from an ordinary
    //    divergence. Checked *before* ancestry for that reason. Preferring the
    //    branch here can never lose work, because replayed commits are
    //    rewrites of commits the branch already has.
    if let Some(tip) = &tip
        && rebase_in_progress(workdir).await
    {
        reconciling(
            tx,
            format!(
                "a rebase is in progress on a detached HEAD; taking {branch} at {}, which still \
                 holds the complete pre-rebase history",
                short(tip)
            ),
        )
        .await;
        return take_branch(workdir, branch, tx).await;
    }

    // 4. The branch ref is gone — the agent deleted or renamed it.
    let Some(tip) = tip else {
        reconciling(
            tx,
            format!(
                "{branch} no longer exists; recreating it at HEAD {}",
                short(&head)
            ),
        )
        .await;
        take_head(workdir, branch, &head).await?;
        return Ok(None);
    };

    // 5. The branch is an ancestor of HEAD (or equal): the agent moved on and
    //    left the ref behind. Nothing is abandoned — the tip is reachable.
    if is_ancestor(workdir, &tip, &head).await {
        reconciling(
            tx,
            format!(
                "{branch} at {} is behind HEAD {}; moving the branch to HEAD",
                short(&tip),
                short(&head)
            ),
        )
        .await;
        take_head(workdir, branch, &head).await?;
        return Ok(None);
    }

    // 6. HEAD is a stale checkout; the branch already holds the work.
    if is_ancestor(workdir, &head, &tip).await {
        reconciling(
            tx,
            format!(
                "HEAD {} is a stale checkout of {branch} {}; taking the branch",
                short(&head),
                short(&tip)
            ),
        )
        .await;
        return take_branch(workdir, branch, tx).await;
    }

    // 7. Diverged: the agent rewrote its history from a detached HEAD, and
    //    where it finished is HEAD. The old tip rides along abandoned.
    reconciling(
        tx,
        format!(
            "HEAD {} and {branch} {} have diverged (the agent rewrote its history); taking HEAD \
             and keeping the old tip as refs/abandoned/{branch}",
            short(&head),
            short(&tip)
        ),
    )
    .await;
    take_head(workdir, branch, &head).await?;
    Ok(Some(tip))
}

/// A reconciliation line, in the build transcript (#825). Every case but the
/// common one says what it did, so a reviewer reads it there instead of
/// re-deriving it from SHAs in an error string.
async fn reconciling(tx: &mpsc::Sender<TaskVmEvent>, line: String) {
    progress(
        tx,
        format!("builder-supervisor: reconciling the build branch: {line}"),
    )
    .await;
}

/// Point the build branch at `head` and reattach HEAD to it.
///
/// `git symbolic-ref` and not `git checkout`: the branch now points at the
/// commit that is already checked out, so nothing about the index or the
/// worktree may change — and a `checkout` with local modifications can refuse
/// outright. The sweep that follows is then byte-for-byte the ordinary one.
async fn take_head(workdir: &Path, branch: &str, head: &str) -> Result<()> {
    let branch_ref = format!("refs/heads/{branch}");
    git(workdir, &["update-ref", &branch_ref, head])
        .await
        .context("update-ref")?;
    git(workdir, &["symbolic-ref", "HEAD", &branch_ref])
        .await
        .context("symbolic-ref")?;
    Ok(())
}

/// Move to the build branch, keeping whatever the checkout being left holds.
///
/// The dirty worktree is committed **first**: those changes belong to the
/// checkout the agent is standing on, not to the branch, and the `checkout
/// --force` below would otherwise delete them. Moving to a *different* commit
/// is exactly the case that needs `--force`, which is why the commit comes
/// first rather than the other way round.
///
/// Returns the tip left behind, when it is not reachable from the branch.
async fn take_branch(
    workdir: &Path,
    branch: &str,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<Option<String>> {
    let stranded = commit_worktree(workdir, STRANDED_MESSAGE)
        .await
        .context("stranded commit")?;
    let left = git_stdout(workdir, &["rev-parse", "HEAD"])
        .await
        .ok()
        .filter(|s| !s.is_empty());
    if stranded && let Some(sha) = &left {
        reconciling(
            tx,
            format!(
                "work the agent left uncommitted off the build branch was committed at {} and \
                 rides the bundle as refs/abandoned/{branch}",
                short(sha)
            ),
        )
        .await;
    }
    // Best-effort: an abort restores HEAD to the branch and clears the replay
    // state. Failing that, the `--force` checkout below still lands.
    if rebase_in_progress(workdir).await {
        let _ = git(workdir, &["rebase", "--abort"]).await;
    }
    git(workdir, &["checkout", "--force", branch])
        .await
        .context("checkout")?;
    let now = git_stdout(workdir, &["rev-parse", "HEAD"])
        .await
        .context("rev-parse after checkout")?;
    Ok(match left {
        Some(left) if !is_ancestor(workdir, &left, &now).await => Some(left),
        _ => None,
    })
}

/// Commit everything the worktree holds onto the current HEAD; `true` if there
/// was anything to commit.
///
/// Shared by the sweep and by the reconciliation's rescue of stranded work —
/// losing a build to a forgotten `git commit` is a bad failure mode wherever
/// the agent left it.
async fn commit_worktree(workdir: &Path, message: &str) -> Result<bool> {
    git(workdir, &["add", "-A"]).await.context("add")?;
    let staged = git(workdir, &["diff", "--cached", "--quiet"])
        .await
        .is_err();
    if staged {
        git(workdir, &["commit", "-m", message])
            .await
            .context("commit")?;
    }
    Ok(staged)
}

/// Whether `ancestor` is reachable from `descendant` (equal counts).
///
/// Anything but exit 0 is "no" — an unreadable answer routes to the arm that
/// keeps both tips rather than to one that drops a commit.
async fn is_ancestor(workdir: &Path, ancestor: &str, descendant: &str) -> bool {
    git(
        workdir,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .await
    .is_ok()
}

/// Whether git is part-way through a rebase. Merge, cherry-pick and revert
/// leave HEAD attached to the branch, so only this one can reach the
/// divergence arm.
async fn rebase_in_progress(workdir: &Path) -> bool {
    let Ok(git_dir) = git_stdout(workdir, &["rev-parse", "--git-dir"]).await else {
        return false;
    };
    // `--git-dir` answers relatively (`.git`) from a worktree root; joining an
    // absolute answer onto the workdir replaces it.
    let git_dir = workdir.join(git_dir);
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

/// Bundle `base_sha..refs/heads/<branch>` — plus the abandoned tip when there
/// is one — and read the branch's tip back **out of the bundle**.
///
/// That read is the second half of #891's fix: `head_sha` used to be a second,
/// independently observed value that had to agree with what was packaged, and
/// the two came from different refs. There is one value now, and it describes
/// the bytes the server actually receives.
async fn package_bundle(
    workdir: &Path,
    base_sha: &str,
    branch: &str,
    abandoned: &[String],
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<(String, String)> {
    let branch_ref = format!("refs/heads/{branch}");
    let bundle_path = workdir.join(".git").join("egress.bundle");
    let bundle_arg = bundle_path.display().to_string();

    // A slice and not an `Option`: the reconciliation runs once per round, so a
    // repair round can strand a second tip, and one ref name could only hold
    // one of them. `-2`, `-3`… rather than `<branch>/2`, because git cannot
    // hold both a ref and a directory at one path.
    let mut abandoned_refs = Vec::new();
    for (i, sha) in abandoned.iter().enumerate() {
        let name = match i {
            0 => format!("refs/abandoned/{branch}"),
            n => format!("refs/abandoned/{branch}-{}", n + 1),
        };
        match git(workdir, &["update-ref", &name, sha]).await {
            Ok(()) => abandoned_refs.push(name),
            Err(e) => {
                progress(
                    tx,
                    format!(
                        "builder-supervisor: could not record the abandoned tip {} as {name}: {e}",
                        short(sha)
                    ),
                )
                .await;
            }
        }
    }

    loop {
        let mut args = vec![
            "bundle".to_string(),
            "create".to_string(),
            bundle_arg.clone(),
            format!("{base_sha}..{branch_ref}"),
        ];
        for name in &abandoned_refs {
            args.push(format!("{base_sha}..{name}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();

        let attempt = async {
            git(workdir, &argv).await.context("bundle create")?;
            let bytes = tokio::fs::read(&bundle_path).await.context("bundle read")?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            anyhow::ensure!(
                encoded.len() <= MAX_BUNDLE_BASE64_BYTES,
                "bundle too large: {} bytes encoded (cap {MAX_BUNDLE_BASE64_BYTES})",
                encoded.len()
            );
            Ok::<String, anyhow::Error>(encoded)
        }
        .await;

        match attempt {
            Ok(bundle_base64) => {
                let tip = bundle_head(workdir, &bundle_path, &branch_ref).await?;
                return Ok((bundle_base64, tip));
            }
            // Insurance must never cost the thing it insures: drop the
            // abandoned refs and ship the branch alone.
            Err(e) if !abandoned_refs.is_empty() => {
                progress(
                    tx,
                    format!(
                        "builder-supervisor: could not ship {} alongside the build branch ({e}); \
                         shipping the branch alone",
                        abandoned_refs.join(", ")
                    ),
                )
                .await;
                abandoned_refs.clear();
            }
            Err(e) => return Err(e),
        }
    }
}

/// The tip `git bundle create` actually packaged for `ref_name`.
///
/// `git bundle list-heads` prints one `<sha> <ref>` line per packaged ref, so
/// this selects on the ref name; taking the first line would read the
/// abandoned tip whenever one rides along.
async fn bundle_head(workdir: &Path, bundle: &Path, ref_name: &str) -> Result<String> {
    let listing = git_stdout(
        workdir,
        &["bundle", "list-heads", &bundle.display().to_string()],
    )
    .await
    .context("bundle list-heads")?;
    listing
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim() == ref_name)
        .map(|(sha, _)| sha.trim().to_string())
        .with_context(|| format!("{ref_name} is not in the bundle"))
}

/// What one run of the project's suite came back with.
///
/// Two arms and not four, because red is structurally different from every
/// other answer: [`VerificationStatus`] has no `Failed` variant, so a red suite
/// has no status to report and cannot be packaged. Everything else — a pass, or
/// one of the several ways there is no evidence — is a [`Verification`] the
/// build ships.
#[derive(Debug)]
enum SuiteResult {
    /// The declared suite ran and failed. `tail` is what the repair round is
    /// shown.
    Red { detail: String, tail: String },
    /// A pass, or an honest absence of one.
    Reported(Verification),
}

/// Bytes of suite output kept for the repair round.
///
/// The **tail**, not the head: every plausible runner puts its failures and its
/// summary at the end, and a head would hand the agent the compile lines it
/// already knows about.
const SUITE_TAIL_BYTES: usize = 16 * 1024;

/// How long to wait for the output readers after the suite's own process has
/// exited.
///
/// Bounded rather than awaited, for the smaller size of the same hazard that
/// makes the timeout path abort them outright: a suite that leaves a daemon
/// behind leaves that daemon holding the pipes, so EOF never arrives and an
/// unbounded await here would hang a build that had already finished.
const COLLECT_GRACE: Duration = Duration::from_secs(5);

/// Run the project's declared test suite against the swept tree, and say what
/// it found.
///
/// # The gate is read at the BASE commit, never at the tip
///
/// The Builder agent has write access to this worktree. A tip-resolved gate is
/// the same forgery this check exists to prevent, one level down, with `exit 0`
/// in place of `PASSED`. The argument for tip-resolution — "a pull request that
/// changes how the project is tested changes its own gate" — is a property of
/// GitHub Actions, where a human reviews the diff *before* the gate matters,
/// and it inverts here: this gate decides whether a pull request is opened at
/// all, so the reviewer only ever sees the diff **after** it has ruled.
///
/// A branch that edits the script is *reported* rather than refused. Changing
/// how a project is tested is ordinary work; the reviewer needs to know which
/// script ruled, not to be blocked.
///
/// # Which gate ruled is always reported
///
/// `detail` always names the blob SHA of the script that ran, matching or not —
/// a field that appears only on disagreement is one nobody learns to read. The
/// comparison is against the **trunk's** copy and not against this build's own
/// diff, because this pipeline stacks builds routinely: build A weakens the
/// script and opens a PR, build B is dispatched onto A's branch, and the
/// weakened script is already in B's base commit. Comparing against the base is
/// exactly what would miss that. It is best-effort — a trunk that is not in the
/// clone is reported as an unmade comparison, never as agreement.
async fn run_verification(
    workdir: &Path,
    base_sha: &str,
    trunk_branch: Option<&str>,
    remaining_secs: Option<u64>,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> SuiteResult {
    let spec = format!("{base_sha}:{VERIFY_SCRIPT_PATH}");
    // `git show` fails identically for "no such path" and "no such commit", and
    // the second cannot happen here — `base_sha` was read out of this clone a
    // few steps ago — so the failure is read as "the project declares nothing".
    let Ok(script) = git_stdout_raw(workdir, &["show", &spec]).await else {
        verifying(
            tx,
            format!(
                "this project declares no {VERIFY_SCRIPT_PATH} at its base commit; nothing to run"
            ),
        )
        .await;
        return SuiteResult::Reported(Verification::new(
            VerificationStatus::Undeclared,
            format!(
                "the project declares no {VERIFY_SCRIPT_PATH} at {}",
                short(base_sha)
            ),
        ));
    };
    // An empty script is the cheapest possible forgery: `sh` on an empty file
    // exits 0, so reading it as a pass would ship anything.
    if script.trim().is_empty() {
        verifying(
            tx,
            format!("{VERIFY_SCRIPT_PATH} is empty, which `sh` exits 0 on; refusing to read that as a pass"),
        )
        .await;
        return SuiteResult::Reported(Verification::new(
            VerificationStatus::Undeclared,
            format!("{VERIFY_SCRIPT_PATH} is empty, and an empty script is not a passing run"),
        ));
    }

    let gate = gate_identity(workdir, base_sha, trunk_branch, tx).await;

    let budget = match suite_budget_secs(remaining_secs, suite_budget_cap_from_env()) {
        SuiteBudget::Run(budget) => budget,
        SuiteBudget::Skip(mut skipped) => {
            verifying(tx, format!("not running the suite: {}", skipped.detail)).await;
            skipped.detail = format!("{} ({gate})", skipped.detail);
            return SuiteResult::Reported(skipped);
        }
    };

    // Staged OUTSIDE the worktree — inside `.git`, which `git add -A` never
    // sees and a `checkout --force` never touches. Writing it into the tree
    // would make the sweep commit the gate into the branch it is judging.
    let staged = workdir.join(".git").join("tasks-verify");
    if let Err(e) = tokio::fs::write(&staged, script.as_bytes()).await {
        return SuiteResult::Reported(Verification::new(
            VerificationStatus::Unavailable,
            format!("{VERIFY_SCRIPT_PATH} could not be staged for running: {e} ({gate})"),
        ));
    }

    verifying(
        tx,
        format!(
            "running {VERIFY_SCRIPT_PATH} ({gate}) with a budget of {}s",
            budget.as_secs()
        ),
    )
    .await;

    // Always `sh <script>`: the shebang is decorative and the executable bit is
    // deliberately not consulted, because honouring it would mean two
    // invocation paths that can drift and a mode bit a `git apply` can drop
    // silently. cwd is the repo root, which is what a project's own suite
    // expects.
    let script_arg = staged.display().to_string();
    match run_script(workdir, &["sh", &script_arg], budget, tx).await {
        ScriptOutcome::Exited { success: true, .. } => {
            verifying(tx, format!("{VERIFY_SCRIPT_PATH} passed ({gate})")).await;
            SuiteResult::Reported(Verification::new(
                VerificationStatus::Passed,
                format!("{VERIFY_SCRIPT_PATH} passed ({gate})"),
            ))
        }
        ScriptOutcome::Exited { code, tail, .. } => {
            verifying(
                tx,
                format!("{VERIFY_SCRIPT_PATH} FAILED with {code} ({gate})"),
            )
            .await;
            SuiteResult::Red {
                detail: format!("{VERIFY_SCRIPT_PATH} exited with {code} ({gate})"),
                tail,
            }
        }
        // Not a failure of the build: a suite that never finished is not
        // evidence about the work, and throwing away a possibly-perfect
        // implementation because a cold `target/` compiled slowly is the
        // failure #929 and #884 were filed about. Ships, and is never green.
        ScriptOutcome::TimedOut => {
            verifying(
                tx,
                format!(
                    "{VERIFY_SCRIPT_PATH} did not finish inside {}s and was killed; the \
                     build ships with no passing run behind it",
                    budget.as_secs()
                ),
            )
            .await;
            SuiteResult::Reported(Verification::new(
                VerificationStatus::TimedOut,
                format!(
                    "{VERIFY_SCRIPT_PATH} was killed after {}s ({gate})",
                    budget.as_secs()
                ),
            ))
        }
        ScriptOutcome::NotStarted(e) => {
            verifying(tx, format!("{VERIFY_SCRIPT_PATH} could not be run: {e}")).await;
            SuiteResult::Reported(Verification::new(
                VerificationStatus::Unavailable,
                format!("{VERIFY_SCRIPT_PATH} could not be run: {e} ({gate})"),
            ))
        }
    }
}

/// Which gate ruled, as a clause every `detail` carries.
///
/// Always names this build's own blob SHA. The trunk half is best-effort and
/// says so when it could not be made — "the comparison was not available" and
/// "the scripts agree" are different facts, and only one of them is reassuring.
async fn gate_identity(
    workdir: &Path,
    base_sha: &str,
    trunk_branch: Option<&str>,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> String {
    let blob = git_stdout(
        workdir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base_sha}:{VERIFY_SCRIPT_PATH}"),
        ],
    )
    .await
    .ok()
    .filter(|s| !s.is_empty());
    let Some(blob) = blob else {
        // The script was readable a moment ago, so this is a git that answered
        // one question and not the other. Say so rather than inventing an id.
        return "gate unidentified".to_string();
    };
    let gate = format!("gate {}", short(&blob));

    let Some(trunk) = trunk_branch else {
        return format!("{gate}, not compared against the trunk (the server named none)");
    };
    let trunk_blob = git_stdout(
        workdir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("origin/{trunk}:{VERIFY_SCRIPT_PATH}"),
        ],
    )
    .await
    .ok()
    .filter(|s| !s.is_empty());
    match trunk_blob {
        Some(trunk_blob) if trunk_blob == blob => format!("{gate}, same as {trunk}"),
        Some(trunk_blob) => {
            // `declaration_changed`, raised on the right comparison. Reported
            // and never refused — see [`run_verification`].
            verifying(
                tx,
                format!(
                    "declaration_changed: the {VERIFY_SCRIPT_PATH} that ruled ({}) is NOT \
                     the one on {trunk} ({}); this build was gated by a script the trunk does \
                     not have",
                    short(&blob),
                    short(&trunk_blob)
                ),
            )
            .await;
            format!("{gate}, DIFFERS from {trunk}'s {}", short(&trunk_blob))
        }
        None => format!("{gate}, {trunk} not reachable in this clone so no comparison was made"),
    }
}

/// A verification line, in the build transcript (#825).
async fn verifying(tx: &mpsc::Sender<TaskVmEvent>, line: String) {
    progress(tx, format!("builder-supervisor: verification: {line}")).await;
}

/// How a script run ended.
#[derive(Debug)]
enum ScriptOutcome {
    Exited {
        success: bool,
        code: String,
        tail: String,
    },
    TimedOut,
    NotStarted(String),
}

/// Run a command under a budget, streaming both pipes into the transcript and
/// keeping a bounded tail.
///
/// # The collector must not be awaited on the timeout path
///
/// Killing `sh` does not close the pipes its children inherited, so the readers
/// never see EOF and an await here hangs the supervisor **forever** — strictly
/// worse than the timeout it was reporting. Found by a test that hung.
/// `collector.abort()` is the fix, and the tail lives outside the task so
/// aborting it costs nothing that was already collected.
async fn run_script(
    workdir: &Path,
    argv: &[&str],
    budget: Duration,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> ScriptOutcome {
    let Some((prog, args)) = argv.split_first() else {
        return ScriptOutcome::NotStarted("empty command".into());
    };
    let mut child = match Command::new(prog)
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return ScriptOutcome::NotStarted(format!("spawn {prog}: {e}")),
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return ScriptOutcome::NotStarted("could not capture the script's output".into());
    };

    let tail = Arc::new(Mutex::new(Tail::new(SUITE_TAIL_BYTES)));
    let collector = tokio::spawn({
        let tail = tail.clone();
        let tx = tx.clone();
        async move {
            let out = pump(stdout, LogStream::Stdout, tail.clone(), tx.clone());
            let err = pump(stderr, LogStream::Stderr, tail, tx);
            tokio::join!(out, err);
        }
    });

    let outcome = tokio::select! {
        status = child.wait() => Some(status),
        _ = tokio::time::sleep(budget) => None,
    };

    match outcome {
        Some(status) => {
            // Bounded, never unbounded — see the doc comment.
            if tokio::time::timeout(COLLECT_GRACE, collector)
                .await
                .is_err()
            {
                warn!("the suite left something holding its pipes; reporting without waiting");
            }
            let tail = tail.lock().map(|t| t.take()).unwrap_or_default();
            match status {
                Ok(status) => ScriptOutcome::Exited {
                    success: status.success(),
                    code: describe_status(&status),
                    tail,
                },
                Err(e) => ScriptOutcome::NotStarted(format!("wait: {e}")),
            }
        }
        None => {
            let _ = child.kill().await;
            collector.abort();
            ScriptOutcome::TimedOut
        }
    }
}

/// Forward one pipe into the transcript, keeping every line in `tail` too.
async fn pump(
    pipe: impl tokio::io::AsyncRead + Unpin,
    stream: LogStream,
    tail: Arc<Mutex<Tail>>,
    tx: mpsc::Sender<TaskVmEvent>,
) {
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(mut tail) = tail.lock() {
            tail.push_line(&line);
        }
        emit(&tx, BuildEvent::Progress { stream, line }).await;
    }
}

/// An exit status, as a clause. A signal death has no code at all, which is
/// exactly the case a bare `code.unwrap_or(-1)` would render as a lie.
fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => format!("no exit code ({status})"),
    }
}

/// The last `cap` bytes of a stream, kept whole lines at a time.
#[derive(Debug, Default)]
struct Tail {
    buf: String,
    cap: usize,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            buf: String::new(),
            cap,
        }
    }

    fn push_line(&mut self, line: &str) {
        self.buf.push_str(line);
        self.buf.push('\n');
        // Drop whole lines from the front: a tail cut mid-line reads as a
        // corrupt first line to whoever is handed it.
        while self.buf.len() > self.cap {
            match self.buf.find('\n') {
                Some(i) => drop(self.buf.drain(..=i)),
                None => {
                    self.buf.clear();
                    break;
                }
            }
        }
    }

    fn take(&self) -> String {
        self.buf.clone()
    }
}

/// A SHA, shortened for a log line.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

fn make_workdir(build_id: &str) -> Result<PathBuf> {
    let base = std::env::var("BUILDER_WORKDIR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!(
        "build-{build_id}-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    std::fs::create_dir_all(&dir).context("create workdir")?;
    Ok(dir)
}

async fn git(workdir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.first().unwrap_or(&"")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} exited with {}: {}",
            redact(&args.join(" ")),
            output.status,
            redact(String::from_utf8_lossy(&output.stderr).trim())
        );
    }
    Ok(())
}

async fn git_stdout(workdir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.first().unwrap_or(&"")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} exited with {}: {}",
            redact(&args.join(" ")),
            output.status,
            redact(String::from_utf8_lossy(&output.stderr).trim())
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git_stdout`, keeping stdout byte-for-byte.
///
/// Separate from [`git_stdout`] because that one trims, and this one's caller
/// writes the bytes back out as a script: a trailing heredoc terminator or a
/// deliberate final newline is not whitespace to be tidied away.
async fn git_stdout_raw(workdir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.first().unwrap_or(&"")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} exited with {}: {}",
            redact(&args.join(" ")),
            output.status,
            redact(String::from_utf8_lossy(&output.stderr).trim())
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run the agent to a conclusion, resuming it across dropped API connections,
/// and report how the whole run ended — including whether anything in this VM
/// was OOM-killed while it ran. See [`tasks_protocol::vm_memory`] for why the
/// exit code alone cannot answer that.
///
/// The resume loop is #845's fix, and it matters more here than for a scout:
/// resuming happens **in this VM**, so the conversation *and* the worktree
/// survive. A host-side retry would get a new VM and a fresh clone — and for a
/// Builder, that worktree is the implementation. Everything about when not to
/// resume lives in [`tasks_protocol::agent_run`].
async fn run_agent(
    workdir: &Path,
    prompt: &str,
    tx: mpsc::Sender<TaskVmEvent>,
) -> Result<AgentRun> {
    let argv = agent_argv()?;
    agent_loop(workdir, &argv, argv.clone(), prompt.to_string(), tx).await
}

/// The operator's configured agent command.
fn agent_argv() -> Result<Vec<String>> {
    let cmd_str = std::env::var("BUILDER_AGENT_CMD").unwrap_or_else(|_| "claude --print".into());
    let argv: Vec<String> = cmd_str.split_whitespace().map(str::to_string).collect();
    anyhow::ensure!(!argv.is_empty(), "BUILDER_AGENT_CMD is empty");
    Ok(argv)
}

/// Hand a red test suite back to the agent that wrote it, once.
///
/// Goes through `--resume` on the **same conversation and the same worktree**,
/// which is what makes one round enough to be worth having: the agent still has
/// everything it built, and is told what broke rather than asked to start over.
///
/// # It shares `BUILDER_MAX_RESUMES`'s mechanism and must never share its
/// counter
///
/// They answer different questions. `BUILDER_MAX_RESUMES` bounds how many times
/// a **dropped API connection** may be picked back up; this bounds how many
/// times the agent may be told **its own tests are red**. So a build that
/// already spent both resumes on dropped connections still gets its repair
/// round, and a repair round that itself dies mid-response is still resumed —
/// it goes through the same [`agent_loop`], which owns that ladder. The total
/// is bounded by the wall budget, which is the right bound for it.
///
/// One round, hardcoded, and not an env knob: an unbounded repair loop burns
/// the budget with nothing that stops it, and an agent that cannot make its own
/// tests pass in two attempts has produced a verdict.
async fn repair_round(
    workdir: &Path,
    session_id: &str,
    detail: &str,
    tail: &str,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<AgentRun> {
    let argv = agent_argv()?;
    // The same guard [`decide`] applies, asked here rather than restated:
    // appending a selector beside one the operator already chose would change
    // which conversation runs.
    if let Some(flag) = command_selects_session(&argv) {
        anyhow::bail!("the configured agent command already selects a session ({flag})");
    }
    let line = format!(
        "builder-supervisor: the test suite failed; handing it back to the agent for one repair \
         round on session {session_id}"
    );
    warn!(%line, "repairing a red suite");
    progress(tx, line).await;
    agent_loop(
        workdir,
        &argv,
        resume_argv(&argv, session_id),
        repair_prompt(detail, tail),
        tx.clone(),
    )
    .await
}

/// What the agent is told about its own red suite.
///
/// It deliberately **does not restate the specs**. That is [`RESUME_PROMPT`]'s
/// rule and it is the whole reason a resume is not a restart: the task is above
/// this in the conversation, and re-sending it is how a resume silently becomes
/// one.
///
/// Two things are said outright rather than left to be discovered. That editing
/// the gate cannot help — an agent that does not know the script is read at the
/// base commit will spend its one round finding out. And that deleting,
/// skipping or `#[ignore]`-ing a failing test is the one repair that makes the
/// whole check worthless — the thing an agent optimising for a green exit code
/// reaches for first.
fn repair_prompt(detail: &str, tail: &str) -> String {
    format!(
        "STOP. Your implementation is finished but the project's test suite does not pass.\n\n\
         This is not your own test run and not a claim from your summary: the supervisor ran \
         `{VERIFY_SCRIPT_PATH}` itself, just now, against the committed tree your branch \
         actually carries. It failed: {detail}\n\n\
         The last of its output:\n\n```\n{tail}\n```\n\n\
         You have EXACTLY ONE attempt to fix this. If the suite is still red after it, this \
         build fails and no pull request is opened — nothing you have written reaches anyone.\n\n\
         Three things to know before you start:\n\
         1. Editing `{VERIFY_SCRIPT_PATH}` cannot help you. It is read out of the build's BASE \
         commit, so your version of it is never the one that runs.\n\
         2. Deleting a failing test, skipping it, or marking it `#[ignore]` is the one repair \
         that makes this whole check worthless. Do not. If a test is genuinely wrong, fix the \
         test and say so plainly in `SUMMARY.md`.\n\
         3. If you change or add tests, account for that in `SUMMARY.md` — it is the pull \
         request body, and the reviewer reads it rather than this message.\n\n\
         Fix the failure, commit it, and stop.",
    )
}

/// Run the agent to a conclusion, resuming it across dropped API connections.
///
/// `base_argv` is the operator's configured command, which is what [`decide`]
/// reads; `first_argv` is what this particular invocation starts with, and
/// differs from it only for a repair round (which starts already aimed at a
/// session).
async fn agent_loop(
    workdir: &Path,
    base_argv: &[String],
    first_argv: Vec<String>,
    first_input: String,
    tx: mpsc::Sender<TaskVmEvent>,
) -> Result<AgentRun> {
    let argv = base_argv.to_vec();
    let max_resumes = max_resumes_from_env("BUILDER_MAX_RESUMES");

    let before = sample_memory();
    let mut attempt_argv = first_argv;
    let mut input = first_input;
    let mut resumes = 0u32;

    loop {
        let (outcome, watcher) = spawn_attempt(&attempt_argv, workdir, &input, before, &tx).await?;

        match tasks_protocol::agent_run::decide(&watcher, &outcome, &argv, resumes, max_resumes) {
            ResumeDecision::Resume {
                argv: next,
                delay,
                attempt,
            } => {
                // Resume boundaries go out as stderr Progress lines, which is
                // how they reach the build transcript (#825).
                let line = format!(
                    "builder-supervisor: the agent's API connection dropped; resuming session \
                     {} in {}s (resume {attempt} of {max_resumes})",
                    watcher.session_id().unwrap_or("?"),
                    delay.as_secs()
                );
                warn!(%line, "resuming agent");
                progress(&tx, line).await;
                tokio::time::sleep(delay).await;
                attempt_argv = next;
                input = RESUME_PROMPT.to_string();
                resumes = attempt;
            }
            ResumeDecision::Stop(no_resume) => {
                let ending = watcher.ending();
                if ending.is_transport() {
                    let line = format!(
                        "builder-supervisor: the agent's API connection dropped and the run was \
                         not resumed — {}",
                        no_resume.describe()
                    );
                    warn!(%line, "agent run ending without a resume");
                    progress(&tx, line).await;
                }
                // Every run, not just the failing ones — an agent that exits 0
                // after the OOM killer ate its build is the case this is here
                // to catch.
                if let Some(summary) = outcome.memory_summary() {
                    info!(%summary, "VM memory");
                    progress(&tx, format!("builder-supervisor: VM memory: {summary}")).await;
                }
                return Ok(AgentRun {
                    outcome,
                    ending,
                    resumes,
                    no_resume: Some(no_resume),
                    // Carried off the run so the repair round has a
                    // conversation to hand a red suite back to. `None` is an
                    // agent that never announced one, and the only honest
                    // response to that is not to resume.
                    session_id: watcher.session_id().map(str::to_string),
                });
            }
        }
    }
}

/// A stderr `Progress` line from the supervisor itself.
async fn progress(tx: &mpsc::Sender<TaskVmEvent>, line: String) {
    emit(
        tx,
        BuildEvent::Progress {
            stream: LogStream::Stderr,
            line,
        },
    )
    .await;
}

/// Run the agent process once, streaming its output.
///
/// The [`ResultWatcher`] rides the *same* stdout loop that forwards `Progress`
/// lines, so what it classifies is byte-for-byte what was reported — and since
/// #825 that is also what lands in the build transcript.
async fn spawn_attempt(
    argv: &[String],
    workdir: &Path,
    input: &str,
    before: Option<MemorySample>,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<(AgentOutcome, ResultWatcher)> {
    let (prog, args) = argv.split_first().context("BUILDER_AGENT_CMD is empty")?;
    info!(?prog, ?args, workdir = %workdir.display(), "running agent");

    let mut child = Command::new(prog)
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn agent")?;

    let mut stdin = child.stdin.take().context("agent stdin")?;
    let input_owned = input.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(input_owned.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.flush().await;
        drop(stdin);
    });

    let stdout = child.stdout.take().context("agent stdout")?;
    let stderr = child.stderr.take().context("agent stderr")?;

    let tx_out = tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut watcher = ResultWatcher::new();
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = r.next_line().await {
            watcher.observe(&line);
            emit(
                &tx_out,
                BuildEvent::Progress {
                    stream: LogStream::Stdout,
                    line,
                },
            )
            .await;
        }
        watcher
    });
    let tx_err = tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = r.next_line().await {
            emit(
                &tx_err,
                BuildEvent::Progress {
                    stream: LogStream::Stderr,
                    line,
                },
            )
            .await;
        }
    });

    let status = child.wait().await.context("wait for agent")?;
    // The stdout side now carries a value, so it cannot be discarded. A
    // panicked reader degrades to a default watcher — `Silent`, which resumes
    // nothing — rather than taking the run down with it.
    let watcher = stdout_task.await.unwrap_or_default();
    let _ = stderr_task.await;

    Ok((AgentOutcome::new(status, before, sample_memory()), watcher))
}
