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
//!      from the worktree and sweeps everything else the agent left
//!      uncommitted into a final commit — losing a build to a forgotten
//!      `git commit` is a bad failure mode
//!   7. `head == base` → `Failed` (the analogue of a scout's missing SPEC.md);
//!      otherwise emits `Completed` with a base64 thin bundle of
//!      `base_sha..branch`
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
    BuildCommand, BuildEvent, LogStream, MAX_BUNDLE_BASE64_BYTES, TaskCommand, TaskEvent,
    TasksProtocol,
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
    macro_rules! fail {
        ($($arg:tt)*) => {{
            emit(&tx, BuildEvent::Failed { reason: format!($($arg)*) }).await;
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

    // Remove the artifacts from the worktree, then sweep. `git add -A` stages
    // the removals as deletions if the agent committed them, alongside any
    // implementation work the agent left uncommitted.
    for artifact in ["PROMPT.md", "SUMMARY.md"] {
        let _ = tokio::fs::remove_file(workdir.join(artifact)).await;
    }
    if let Err(e) = git(&workdir, &["add", "-A"]).await {
        fail!("sweep add: {e}");
    }
    let staged = git(&workdir, &["diff", "--cached", "--quiet"])
        .await
        .is_err();
    if staged
        && let Err(e) = git(
            &workdir,
            &["commit", "-m", "Sweep: work the agent left uncommitted"],
        )
        .await
    {
        fail!("sweep commit: {e}");
    }

    let head_sha = match git_stdout(&workdir, &["rev-parse", "HEAD"]).await {
        Ok(sha) => sha,
        Err(e) => fail!("rev-parse head: {e}"),
    };
    if head_sha == base_sha {
        // "no commits" on its own reads as a verdict on the agent's work, so
        // an OOM kill, a signal death or a dropped API connection (#845) is
        // named here rather than left for someone to infer from a budget that
        // vanished.
        fail!(
            "agent produced no commits (head == base){}",
            run.failure_context()
        );
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

    // Thin bundle with base_sha as its prerequisite, carrying the branch ref
    // the server will fetch by name.
    let bundle_path = workdir.join(".git").join("egress.bundle");
    if let Err(e) = git(
        &workdir,
        &[
            "bundle",
            "create",
            &bundle_path.display().to_string(),
            &format!("{base_sha}..refs/heads/{branch}"),
        ],
    )
    .await
    {
        fail!("bundle: {e}");
    }
    let bundle_bytes = match tokio::fs::read(&bundle_path).await {
        Ok(b) => b,
        Err(e) => fail!("bundle read: {e}"),
    };
    let bundle_base64 = base64::engine::general_purpose::STANDARD.encode(&bundle_bytes);
    if bundle_base64.len() > MAX_BUNDLE_BASE64_BYTES {
        fail!(
            "bundle too large: {} bytes encoded (cap {})",
            bundle_base64.len(),
            MAX_BUNDLE_BASE64_BYTES
        );
    }

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
