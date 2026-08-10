//! scout-supervisor: PID 1 binary that runs inside a Scout VM.
//!
//! Protocol: speaks JSON-lines over stdin/stdout using vm-pool's
//! VmCommand/VmEvent envelopes with [`TasksProtocol`] payloads.
//!
//! On [`ScoutCommand::Start`]:
//!   1. Creates a workdir
//!   2. Clones the repo at `base_branch`, records the base commit SHA
//!   3. Creates a throwaway branch `scout/<task_id>-<short-uuid>`
//!   4. Writes the prompt to `PROMPT.md` (reference copy) and pipes it to the
//!      agent's stdin
//!   5. Runs the configured agent command with cwd = workdir
//!   6. Streams agent stdout/stderr as `ScoutEvent::Progress`
//!   7. On exit: reads `SPEC.md` if present, emits `Completed`; else `Failed`.
//!      A non-zero agent exit with a valid `SPEC.md` still completes — a spec
//!      from a messy exit is still a spec.
//!
//! There is no in-band cancellation: the host cancels a scout by
//! deallocating the VM, which tears down this process and everything under it.
//!
//! The agent command is configured via the `SCOUT_AGENT_CMD` environment
//! variable (tokens space-separated; no shell expansion). Default:
//! `claude --print`. Tests use a trivial shell script instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tasks_protocol::{LogStream, ScoutCommand, ScoutEvent, TaskCommand, TaskEvent, TasksProtocol};
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
    // Logs go to stderr so they don't collide with the stdout JSON stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();
    info!("scout-supervisor starting");

    let (evt_tx, mut evt_rx) = mpsc::channel::<TaskVmEvent>(128);

    // Single stdout writer task — events are serialized in order.
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(event) = evt_rx.recv().await {
            if let Err(e) = write_event(&mut stdout, &event).await {
                error!("failed to write event: {e}");
                break;
            }
        }
    });

    // Ready handshake.
    evt_tx.send(VmEvent::Ready).await.ok();

    // Read commands sequentially. A long-running Start blocks reading the next
    // command — that's OK: Shutdown will arrive via stdin closure (pipe EOF)
    // when the host drops our channel.
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
                    TaskCommand::Scout(ScoutCommand::Start {
                        task_id,
                        repo_clone_url,
                        base_branch,
                        prompt,
                    }),
            } => {
                let tx = evt_tx.clone();
                run_scout(&task_id, &repo_clone_url, &base_branch, &prompt, tx).await;
            }
            // The information barrier's last line: this VM is a Scout, and a
            // Build command is answered with a terminal refusal, never acted on.
            VmCommand::App {
                payload: TaskCommand::Build(_),
            } => {
                warn!("received a build command; this VM is a scout");
                emit(
                    &evt_tx,
                    ScoutEvent::Failed {
                        reason: "this VM is a scout; refusing a build command".into(),
                    },
                )
                .await;
            }
        }
    }

    drop(evt_tx);
    let _ = writer.await;
    info!("scout-supervisor exiting");
    Ok(())
}

async fn write_event(stdout: &mut tokio::io::Stdout, event: &TaskVmEvent) -> Result<()> {
    let json = serde_json::to_string(event)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

async fn emit(tx: &mpsc::Sender<TaskVmEvent>, event: ScoutEvent) {
    let _ = tx
        .send(VmEvent::App {
            payload: TaskEvent::Scout(event),
        })
        .await;
}

/// Run the Scout workflow for a task.
async fn run_scout(
    task_id: &str,
    repo_clone_url: &str,
    base_branch: &str,
    prompt: &str,
    tx: mpsc::Sender<TaskVmEvent>,
) {
    let workdir = match make_workdir(task_id) {
        Ok(w) => w,
        Err(e) => {
            emit(
                &tx,
                ScoutEvent::Failed {
                    reason: format!("workdir: {e}"),
                },
            )
            .await;
            return;
        }
    };
    debug!(task_id, workdir = %workdir.display(), "scout workdir created");

    // 1. Clone
    if let Err(e) = git_clone(repo_clone_url, base_branch, &workdir).await {
        emit(
            &tx,
            ScoutEvent::Failed {
                reason: format!("clone: {e}"),
            },
        )
        .await;
        return;
    }
    // Recorded now because the agent may commit; diffing against HEAD later
    // would miss anything committed.
    let base_sha = match git_rev_parse_head(&workdir).await {
        Ok(sha) => sha,
        Err(e) => {
            emit(
                &tx,
                ScoutEvent::Failed {
                    reason: format!("rev-parse: {e}"),
                },
            )
            .await;
            return;
        }
    };

    // 2. Branch
    let short_id = Uuid::new_v4().simple().to_string()[..8].to_string();
    let branch = format!("scout/{task_id}-{short_id}");
    if let Err(e) = git_checkout_new_branch(&workdir, &branch).await {
        emit(
            &tx,
            ScoutEvent::Failed {
                reason: format!("branch: {e}"),
            },
        )
        .await;
        return;
    }
    emit(
        &tx,
        ScoutEvent::Started {
            branch: branch.clone(),
        },
    )
    .await;

    // 3. Prompt
    if let Err(e) = tokio::fs::write(workdir.join("PROMPT.md"), prompt).await {
        emit(
            &tx,
            ScoutEvent::Failed {
                reason: format!("prompt write: {e}"),
            },
        )
        .await;
        return;
    }

    // 4. Agent
    let exit_code = match run_agent(&workdir, prompt, tx.clone()).await {
        Ok(code) => code,
        Err(e) => {
            emit(
                &tx,
                ScoutEvent::Failed {
                    reason: format!("agent: {e}"),
                },
            )
            .await;
            return;
        }
    };
    emit(&tx, ScoutEvent::ImplementationFinished { exit_code }).await;

    // 5. Spec
    let spec_path = workdir.join("SPEC.md");
    let spec_markdown = match tokio::fs::read_to_string(&spec_path).await {
        Ok(s) => s,
        Err(e) => {
            emit(
                &tx,
                ScoutEvent::Failed {
                    reason: format!("SPEC.md not found at {}: {e}", spec_path.display()),
                },
            )
            .await;
            return;
        }
    };

    let files_touched = match git_diff_name_only(&workdir, &base_sha).await {
        Ok(files) => files,
        Err(e) => {
            warn!("could not compute files_touched: {e}");
            Vec::new()
        }
    };

    emit(
        &tx,
        ScoutEvent::Completed {
            spec_markdown,
            files_touched,
        },
    )
    .await;
}

fn make_workdir(task_id: &str) -> Result<PathBuf> {
    let base = std::env::var("SCOUT_WORKDIR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!(
        "scout-{task_id}-{}",
        Uuid::new_v4().simple().to_string()[..8].to_string()
    ));
    std::fs::create_dir_all(&dir).context("create workdir")?;
    Ok(dir)
}

async fn git_clone(url: &str, branch: &str, into: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--branch", branch, "--depth", "50", url, "."])
        .current_dir(into)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawn git clone")?;
    if !status.success() {
        anyhow::bail!("git clone exited with {status}");
    }
    Ok(())
}

async fn git_checkout_new_branch(workdir: &Path, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(workdir)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawn git checkout")?;
    if !status.success() {
        anyhow::bail!("git checkout -b {branch} exited with {status}");
    }
    Ok(())
}

async fn git_rev_parse_head(workdir: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workdir)
        .output()
        .await
        .context("spawn git rev-parse")?;
    if !output.status.success() {
        anyhow::bail!("git rev-parse exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_diff_name_only(workdir: &Path, base_sha: &str) -> Result<Vec<String>> {
    // Stage everything (tracked + untracked) so we can enumerate all files the
    // agent touched — whether committed or left in the worktree — then diff
    // the index against the recorded base commit.
    let status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(workdir)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawn git add -A")?;
    if !status.success() {
        anyhow::bail!("git add -A exited with {status}");
    }

    let output = Command::new("git")
        .args(["diff", "--name-only", "--cached", base_sha])
        .current_dir(workdir)
        .output()
        .await
        .context("spawn git diff")?;
    if !output.status.success() {
        anyhow::bail!("git diff exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| *l != "PROMPT.md" && *l != "SPEC.md")
        .map(|l| l.to_string())
        .collect())
}

async fn run_agent(workdir: &Path, prompt: &str, tx: mpsc::Sender<TaskVmEvent>) -> Result<i32> {
    let cmd_str = std::env::var("SCOUT_AGENT_CMD").unwrap_or_else(|_| "claude --print".into());
    let mut parts = cmd_str.split_whitespace();
    let prog = parts.next().context("SCOUT_AGENT_CMD is empty")?;
    let args: Vec<&str> = parts.collect();
    info!(?prog, ?args, workdir = %workdir.display(), "running agent");

    let mut child = Command::new(prog)
        .args(&args)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn agent")?;

    // Deliver the prompt on stdin, then close it so the agent sees EOF.
    // Write errors are ignored: an agent that exits without reading (or never
    // reads stdin) is its own kind of failure, surfaced via exit code.
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
                ScoutEvent::Progress {
                    stream: LogStream::Stdout,
                    line,
                },
            )
            .await;
        }
    });
    let tx_err = tx;
    let stderr_task = tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = r.next_line().await {
            emit(
                &tx_err,
                ScoutEvent::Progress {
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
    Ok(status.code().unwrap_or(-1))
}
