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
//!   9. `tip == base` → `Failed` (the analogue of a scout's missing SPEC.md);
//!      otherwise emits `Completed` with a base64 thin bundle of
//!      `base_sha..branch`, whose `head_sha` is read back **out of the
//!      bundle** so there is one value where there used to be two that had to
//!      agree
//!
//! No credential ever needs to enter this VM for egress: the bundle rides the
//! event stream and the server pushes with its own token.
//!
//! The agent command is configured via `BUILDER_AGENT_CMD` (tokens
//! space-separated; no shell expansion). Default: `claude --print`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use base64::Engine as _;
use tasks_protocol::agent_run::{
    AgentRun, RESUME_PROMPT, ResultWatcher, ResumeDecision, max_resumes_from_env,
};
use tasks_protocol::vm_memory::{AgentOutcome, MemorySample, sample_memory};
use tasks_protocol::{
    BuildCommand, BuildEvent, FailureClass, LogStream, MAX_BUNDLE_BASE64_BYTES, TaskCommand,
    TaskEvent, TasksProtocol,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use vm_pool_protocol::{VmCommand, VmEvent};

type TaskVmCommand = VmCommand<TasksProtocol>;
type TaskVmEvent = VmEvent<TasksProtocol>;

#[tokio::main]
async fn main() -> Result<()> {
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
                warn!("invalid command line ({e}): {}", line.trim());
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
                    }),
            } => {
                let tx = evt_tx.clone();
                run_build(
                    &build_id,
                    &repo_clone_url,
                    &base_branch,
                    &branch,
                    &prompt,
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
async fn run_build(
    build_id: &str,
    repo_clone_url: &str,
    base_branch: &str,
    branch: &str,
    prompt: &str,
    tx: mpsc::Sender<TaskVmEvent>,
) {
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

    // SUMMARY.md is read before the artifact cleanup removes it. Optional:
    // missing prose does not fail a build — the code is the deliverable.
    let summary = tokio::fs::read_to_string(workdir.join("SUMMARY.md"))
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Remove the artifacts from the worktree, reconcile, then sweep. `git add
    // -A` stages the removals as deletions if the agent committed them,
    // alongside any implementation work the agent left uncommitted.
    remove_artifacts(&workdir).await;

    // The reconciliation runs BEFORE the sweep, and the order is load-bearing:
    // the sweep commits onto whatever HEAD is, so on a stranded checkout a
    // sweep-first ordering manufactures a divergence no ancestry rule can
    // undo — and the build ships a PR containing the sweep and none of the
    // implementation. See [`reconcile_checkout`].
    let abandoned = match reconcile_checkout(&workdir, branch, &tx).await {
        Ok(abandoned) => abandoned,
        Err(e) => fail!(class: run.failure_class(), "reconcile: {e}"),
    };
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
        // "no commits" on its own reads as a verdict on the agent's work, so
        // an OOM kill, a signal death or a dropped API connection (#845) is
        // named here rather than left for someone to infer from a budget that
        // vanished.
        fail!(
            class: run.failure_class(),
            "agent produced no commits (tip == base){}",
            run.failure_context()
        );
    }

    // Thin bundle with base_sha as its prerequisite, carrying the branch ref
    // the server will fetch by name — and the head is read back out of it.
    let (bundle_base64, head_sha) =
        match package_bundle(&workdir, &base_sha, branch, abandoned.as_deref(), &tx).await {
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
        },
    )
    .await;
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
    abandoned: Option<&str>,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<(String, String)> {
    let branch_ref = format!("refs/heads/{branch}");
    let abandoned_ref = format!("refs/abandoned/{branch}");
    let bundle_path = workdir.join(".git").join("egress.bundle");
    let bundle_arg = bundle_path.display().to_string();

    let mut carry_abandoned = false;
    if let Some(sha) = abandoned {
        match git(workdir, &["update-ref", &abandoned_ref, sha]).await {
            Ok(()) => carry_abandoned = true,
            Err(e) => {
                progress(
                    tx,
                    format!(
                        "builder-supervisor: could not record the abandoned tip {} as \
                         {abandoned_ref}: {e}",
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
        if carry_abandoned {
            args.push(format!("{base_sha}..{abandoned_ref}"));
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
            // abandoned ref and ship the branch alone.
            Err(e) if carry_abandoned => {
                progress(
                    tx,
                    format!(
                        "builder-supervisor: could not ship {abandoned_ref} alongside the build \
                         branch ({e}); shipping the branch alone"
                    ),
                )
                .await;
                carry_abandoned = false;
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
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
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
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
    let cmd_str = std::env::var("BUILDER_AGENT_CMD").unwrap_or_else(|_| "claude --print".into());
    let argv: Vec<String> = cmd_str.split_whitespace().map(str::to_string).collect();
    anyhow::ensure!(!argv.is_empty(), "BUILDER_AGENT_CMD is empty");
    let max_resumes = max_resumes_from_env("BUILDER_MAX_RESUMES");

    let before = sample_memory();
    let mut attempt_argv = argv.clone();
    let mut input = prompt.to_string();
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
