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
//!   5. Runs the configured agent command, streaming stdout/stderr as Progress
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
use tasks_protocol::vm_memory::{AgentOutcome, sample_memory};
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

    let outcome = match run_agent(&workdir, prompt, tx.clone()).await {
        Ok(outcome) => outcome,
        Err(e) => fail!("agent: {e}"),
    };
    emit(
        &tx,
        BuildEvent::ImplementationFinished {
            exit_code: outcome.exit_code,
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
        // A build with no transcript (#825) has only this string to explain
        // itself, so an OOM kill or a signal death is named here rather than
        // left for someone to infer from a budget that vanished.
        fail!(
            "agent produced no commits (head == base){}",
            outcome.failure_context()
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

/// Run the agent, streaming its output, and report how it ended — including
/// whether anything in this VM was OOM-killed while it ran. See
/// [`tasks_protocol::vm_memory`] for why the exit code alone cannot answer
/// that.
async fn run_agent(
    workdir: &Path,
    prompt: &str,
    tx: mpsc::Sender<TaskVmEvent>,
) -> Result<AgentOutcome> {
    let cmd_str = std::env::var("BUILDER_AGENT_CMD").unwrap_or_else(|_| "claude --print".into());
    let mut parts = cmd_str.split_whitespace();
    let prog = parts.next().context("BUILDER_AGENT_CMD is empty")?;
    let args: Vec<&str> = parts.collect();
    info!(?prog, ?args, workdir = %workdir.display(), "running agent");

    let before = sample_memory();
    let mut child = Command::new(prog)
        .args(&args)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn agent")?;

    let mut stdin = child.stdin.take().context("agent stdin")?;
    let prompt_owned = prompt.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt_owned.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.flush().await;
        drop(stdin);
    });

    let stdout = child.stdout.take().context("agent stdout")?;
    let stderr = child.stderr.take().context("agent stderr")?;

    let tx_out = tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = r.next_line().await {
            emit(
                &tx_out,
                BuildEvent::Progress {
                    stream: LogStream::Stdout,
                    line,
                },
            )
            .await;
        }
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
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let outcome = AgentOutcome::new(status, before, sample_memory());
    // Every run, not just the failing ones — an agent that exits 0 after the
    // OOM killer ate its build is the case this is here to catch.
    if let Some(summary) = outcome.memory_summary() {
        info!(%summary, "VM memory");
        emit(
            &tx,
            BuildEvent::Progress {
                stream: LogStream::Stderr,
                line: format!("builder-supervisor: VM memory: {summary}"),
            },
        )
        .await;
    }
    Ok(outcome)
}
