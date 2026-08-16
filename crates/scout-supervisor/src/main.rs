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
//!   5. Runs the configured agent command with cwd = workdir, resuming it in
//!      place (`--resume <session_id>`, up to `SCOUT_MAX_RESUMES`) if its API
//!      connection drops mid-response — see [`tasks_protocol::agent_run`]
//!   6. Streams agent stdout/stderr as `ScoutEvent::Progress`, and `NOTES.md`
//!      as `ScoutEvent::Checkpoint` every `SCOUT_CHECKPOINT_INTERVAL_SECS`
//!   7. On exit: reads `SPEC.md` if present, emits `Completed`; else
//!      `StoppedEarly` if anything was written down, else `Failed`.
//!      A non-zero agent exit with a valid `SPEC.md` still completes — a spec
//!      from a messy exit is still a spec.
//!
//! # Two files, two meanings
//!
//! `SPEC.md` means "I concluded". `NOTES.md` means "here is what I have so
//! far". Keeping them separate is the whole design: reporting a partial spec
//! as a spec would be worse than losing the run, because a half-explored spec
//! entering the review queue looks finished. So notes stream out during the
//! run and are reported as salvage, never as a deliverable.
//!
//! There is no in-band cancellation: the host cancels a scout by
//! deallocating the VM, which tears down this process and everything under it.
//! That is also why checkpoints are pushed as they change rather than
//! collected at the end: at the deadline the VM is destroyed and nothing on
//! its disk survives, so only what the host already received exists.
//!
//! The agent command is configured via the `SCOUT_AGENT_CMD` environment
//! variable (tokens space-separated; no shell expansion). Default:
//! `claude --print`. Tests use a trivial shell script instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tasks_protocol::agent_run::{
    AgentRun, RESUME_PROMPT, ResultWatcher, ResumeDecision, max_resumes_from_env,
};
use tasks_protocol::vm_memory::{AgentOutcome, MemorySample, sample_memory};
use tasks_protocol::{
    LogStream, MAX_NOTES_BYTES, ScoutCommand, ScoutEvent, TaskCommand, TaskEvent, TasksProtocol,
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

    // 4. Agent, watched for checkpoints while it runs.
    let watcher = spawn_checkpoint_watcher(workdir.clone(), tx.clone());
    let agent = run_agent(&workdir, prompt, tx.clone()).await;
    // Stopped before the outcome is reported, so a checkpoint can never arrive
    // after the terminal event that supersedes it.
    watcher.abort();

    let run = match agent {
        Ok(run) => run,
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
    // The exit code of the *last* attempt, so a run that was resumed and then
    // finished cleanly reports 0. This event describes the run, not the first
    // process — see [`AgentRun`].
    emit(
        &tx,
        ScoutEvent::ImplementationFinished {
            exit_code: run.outcome.exit_code,
        },
    )
    .await;

    // 5. Spec, or salvage, or nothing.
    report_outcome(&workdir, &base_sha, &run, &tx).await;
}

/// Decide what the run produced and emit the one terminal event that says so.
///
/// Three outcomes, in descending order of value: a spec, salvage, nothing.
/// The distinction between the last two is not cosmetic — a retry that cannot
/// tell "we salvaged something" from "there was nothing" re-derives what it
/// already had.
async fn report_outcome(
    workdir: &Path,
    base_sha: &str,
    run: &AgentRun,
    tx: &mpsc::Sender<TaskVmEvent>,
) {
    let spec_path = workdir.join(SPEC_FILE);
    let spec = tokio::fs::read_to_string(&spec_path).await;
    let notes = read_notes(workdir).await;

    // Why the run is not being reported as a spec. `None` means it is.
    let shortfall = match &spec {
        Ok(content) => match spec_verdict(content, run.outcome.exit_code) {
            SpecVerdict::Spec => None,
            SpecVerdict::Unfinished { missing } => Some(format!(
                "SPEC.md is not a spec yet — {} {} missing or still template{}",
                missing.join(", "),
                if missing.len() == 1 { "is" } else { "are" },
                run.failure_context()
            )),
        },
        // The most common cause of a missing spec is an agent that never got
        // to write one — including one the OOM killer took out, and one whose
        // API connection died (#845). Say so here: for a Scout this reason is
        // the whole postmortem, and "SPEC.md not found" on its own reads as a
        // verdict on the exploration when it was a dropped connection.
        Err(e) => Some(format!(
            "SPEC.md not found at {}: {e}{}",
            spec_path.display(),
            run.failure_context()
        )),
    };

    // Computed for every reported outcome, not just the successful one: a run
    // that stopped early still touched files, and the next attempt is better
    // off knowing which.
    let files_touched = match git_diff_name_only(workdir, base_sha).await {
        Ok(files) => files,
        Err(e) => {
            warn!("could not compute files_touched: {e}");
            Vec::new()
        }
    };

    let Some(reason) = shortfall else {
        emit(
            tx,
            ScoutEvent::Completed {
                spec_markdown: spec.expect("a spec verdict implies a readable SPEC.md"),
                files_touched,
            },
        )
        .await;
        return;
    };

    let unfinished_spec = spec.ok();
    match render_salvage(notes.as_deref(), unfinished_spec.as_deref()) {
        Some(notes_markdown) => {
            info!(%reason, "run ended without a spec; salvaging what was written down");
            emit(
                tx,
                ScoutEvent::StoppedEarly {
                    reason,
                    notes_markdown,
                    files_touched,
                },
            )
            .await;
        }
        None => {
            emit(tx, ScoutEvent::Failed { reason }).await;
        }
    }
}

/// The file the agent concludes in. Reported as a spec, reviewed as one.
const SPEC_FILE: &str = "SPEC.md";
/// The file the agent thinks in. Streamed back as checkpoints, and reported
/// as salvage when the run ends without a spec. Never a spec.
const NOTES_FILE: &str = "NOTES.md";
/// How often `NOTES.md` is polled when `SCOUT_CHECKPOINT_INTERVAL_SECS` is
/// unset. Far finer than the failure it insures against (a whole scout run),
/// and coarse enough that polling costs nothing.
const DEFAULT_CHECKPOINT_INTERVAL_SECS: u64 = 30;

fn checkpoint_interval() -> Duration {
    let secs = std::env::var("SCOUT_CHECKPOINT_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_CHECKPOINT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Read `NOTES.md`, trimmed to the transport cap. `None` when it is absent,
/// unreadable, or blank — an empty file is not exploration.
async fn read_notes(workdir: &Path) -> Option<String> {
    let content = tokio::fs::read_to_string(workdir.join(NOTES_FILE))
        .await
        .ok()?;
    let content = trim_notes(content);
    (!content.trim().is_empty()).then_some(content)
}

/// Poll `NOTES.md` and push a [`ScoutEvent::Checkpoint`] whenever it changes.
///
/// Polling rather than inotify: it adds no dependency to the agent image, and
/// at 30s granularity the difference is invisible against the thing it
/// insures against. Emitting only on change keeps an idle scout from
/// reprinting a quarter-megabyte every interval.
fn spawn_checkpoint_watcher(
    workdir: PathBuf,
    tx: mpsc::Sender<TaskVmEvent>,
) -> tokio::task::JoinHandle<()> {
    let interval = checkpoint_interval();
    tokio::spawn(async move {
        let mut last: Option<String> = None;
        loop {
            // Sleep first: at t=0 the agent has not written anything yet.
            tokio::time::sleep(interval).await;
            let Some(notes) = read_notes(&workdir).await else {
                continue;
            };
            if last.as_deref() == Some(notes.as_str()) {
                continue;
            }
            debug!(bytes = notes.len(), "checkpointing NOTES.md");
            emit(
                &tx,
                ScoutEvent::Checkpoint {
                    notes_markdown: notes.clone(),
                },
            )
            .await;
            last = Some(notes);
        }
    })
}

/// Cut notes to [`MAX_NOTES_BYTES`] on a char boundary, keeping the head.
/// Notes are written top-down, so a tail-first cut would hand the reader
/// conclusions with nothing to attach them to.
fn trim_notes(notes: String) -> String {
    if notes.len() <= MAX_NOTES_BYTES {
        return notes;
    }
    let mut cut = MAX_NOTES_BYTES;
    while cut > 0 && !notes.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = notes.len() - cut;
    let mut out = notes;
    out.truncate(cut);
    out.push_str(&format!(
        "\n\n…[tasks: NOTES.md truncated, {dropped} bytes dropped]\n"
    ));
    out
}

/// Whether a `SPEC.md` on disk is a spec.
#[derive(Debug, PartialEq, Eq)]
enum SpecVerdict {
    Spec,
    /// Spec-shaped but not spec-substanced: the named sections are absent or
    /// still carry the prompt's template text.
    Unfinished {
        missing: Vec<&'static str>,
    },
}

/// Decide whether `SPEC.md` may be reported as a spec.
///
/// **A clean exit is taken at its word.** An agent that ran to completion and
/// wrote a spec gets no structural audit at all, which is what keeps this
/// change from touching the healthy path: losing a finished spec to a
/// heading-wording quibble would be a worse trade than the trap it prevents.
///
/// Only a messy exit is read sceptically — and it is exactly the messy exit
/// that used to complete a run with any `SPEC.md` at all, however partial.
fn spec_verdict(spec: &str, exit_code: i32) -> SpecVerdict {
    if exit_code == 0 {
        return SpecVerdict::Spec;
    }
    let missing = missing_sections(spec);
    if missing.is_empty() {
        SpecVerdict::Spec
    } else {
        SpecVerdict::Unfinished { missing }
    }
}

/// The template's sections, as (name reported back, keyword matched on).
///
/// Matched by keyword rather than exact heading so real specs — which rename
/// freely — pass: `"blocker"` catches "Blockers and Dependencies", and
/// `"pitfall"` catches "Pitfalls Discovered".
const REQUIRED_SECTIONS: &[(&str, &str)] = &[
    ("Summary", "summary"),
    ("Implementation Approach", "implementation"),
    ("Discovered Pitfalls", "pitfall"),
    ("Blockers & Dependencies", "blocker"),
    ("Complexity", "complexity"),
    ("Notes", "notes"),
];

/// The prompt template's own placeholder lines. An agent that wrote the shape
/// and then died leaves these behind; counting them as content is exactly how
/// a skeleton would get through.
const TEMPLATE_PLACEHOLDERS: &[&str] = &[
    "one paragraph.",
    "bullets: files changed and key design decisions.",
    "edge cases, non-obvious dependencies.",
    "other issues that block this.",
    "simple | medium | complex",
    "anything the builder should know.",
];

/// Template sections that are absent, empty, or still placeholder.
fn missing_sections(spec: &str) -> Vec<&'static str> {
    let sections = parse_sections(spec);
    REQUIRED_SECTIONS
        .iter()
        .filter(|(_, keyword)| {
            !sections
                .iter()
                .any(|(heading, filled)| *filled && heading.contains(keyword))
        })
        .map(|(name, _)| *name)
        .collect()
}

/// Headings and whether each has real content under it.
///
/// Fenced blocks are skipped wholesale: the spec template is itself quoted
/// inside a fence in the prompt, and a `# comment` in a shell snippet is not
/// a heading.
fn parse_sections(spec: &str) -> Vec<(String, bool)> {
    let mut sections: Vec<(String, bool)> = Vec::new();
    let mut fence: Option<String> = None;
    for line in spec.lines() {
        let trimmed = line.trim();
        match &fence {
            Some(open) => {
                if trimmed.starts_with(open.as_str()) {
                    fence = None;
                }
                continue;
            }
            None => {
                if let Some(marker) = fence_marker(trimmed) {
                    fence = Some(marker);
                    continue;
                }
            }
        }

        if let Some(heading) = trimmed.strip_prefix('#') {
            sections.push((heading.trim_start_matches('#').trim().to_lowercase(), false));
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let is_placeholder = TEMPLATE_PLACEHOLDERS.contains(&trimmed.to_lowercase().as_str());
        if let Some((_, filled)) = sections.last_mut() {
            *filled |= !is_placeholder;
        }
    }
    sections
}

/// The fence a line opens, if it opens one.
fn fence_marker(trimmed: &str) -> Option<String> {
    for ch in ['`', '~'] {
        let run = trimmed.chars().take_while(|c| *c == ch).count();
        if run >= 3 {
            return Some(std::iter::repeat_n(ch, run).collect());
        }
    }
    None
}

/// Combine what the run left behind into one salvage document, under
/// headings that say what each part is. `None` when there is nothing.
///
/// The unfinished spec is included but labelled: it is evidence of where the
/// exploration got to, and calling it anything else is the confusion this
/// whole design exists to prevent.
fn render_salvage(notes: Option<&str>, unfinished_spec: Option<&str>) -> Option<String> {
    let notes = notes.map(str::trim).filter(|n| !n.is_empty());
    let spec = unfinished_spec.map(str::trim).filter(|s| !s.is_empty());
    if notes.is_none() && spec.is_none() {
        return None;
    }

    let mut out = String::from(
        "# Salvage from an interrupted scout run\n\n\
         This run ended without concluding. Nothing below is a spec — it is \
         unverified exploration, kept so the next attempt does not start from \
         zero.\n",
    );
    if let Some(notes) = notes {
        out.push_str("\n## NOTES.md (the agent's running notes)\n\n");
        out.push_str(notes);
        out.push('\n');
    }
    if let Some(spec) = spec {
        out.push_str(
            "\n## Unfinished SPEC.md\n\n\
             The agent had started writing a spec but had not finished it. It \
             was not reported as a spec and never entered the review queue.\n\n",
        );
        out.push_str(spec);
        out.push('\n');
    }
    Some(out)
}

fn make_workdir(task_id: &str) -> Result<PathBuf> {
    let base = std::env::var("SCOUT_WORKDIR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!(
        "scout-{task_id}-{}",
        &Uuid::new_v4().simple().to_string()[..8]
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
        .filter(|l| *l != "PROMPT.md" && *l != SPEC_FILE && *l != NOTES_FILE)
        .map(|l| l.to_string())
        .collect())
}

/// Run the agent to a conclusion, resuming it across dropped API connections,
/// and report how the whole run ended.
///
/// The loop is #845's fix. An agent whose connection dies mid-response is
/// re-invoked with `--resume <session_id>` **in this VM**: same conversation,
/// same worktree, same `NOTES.md`. A host-side retry would get a new VM and a
/// fresh clone and lose all three. Everything about when *not* to resume lives
/// in [`tasks_protocol::agent_run`], where it is testable without a process.
///
/// The run is bracketed by cgroup memory samples taken once around the *whole*
/// loop: an OOM kill inside this VM is otherwise invisible, because the kernel
/// usually kills a compiler or linker job the agent merely sees as a failed
/// command. See [`tasks_protocol::vm_memory`].
async fn run_agent(
    workdir: &Path,
    prompt: &str,
    tx: mpsc::Sender<TaskVmEvent>,
) -> Result<AgentRun> {
    let cmd_str = std::env::var("SCOUT_AGENT_CMD").unwrap_or_else(|_| "claude --print".into());
    let argv: Vec<String> = cmd_str.split_whitespace().map(str::to_string).collect();
    anyhow::ensure!(!argv.is_empty(), "SCOUT_AGENT_CMD is empty");
    let max_resumes = max_resumes_from_env("SCOUT_MAX_RESUMES");

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
                // how they reach the transcript — and how often they appear is
                // the direct measurement of how often #845 is happening, which
                // the issue itself could only infer from five runs.
                let line = format!(
                    "scout-supervisor: the agent's API connection dropped; resuming session \
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
                        "scout-supervisor: the agent's API connection dropped and the run was \
                         not resumed — {}",
                        no_resume.describe()
                    );
                    warn!(%line, "agent run ending without a resume");
                    progress(&tx, line).await;
                }
                // Emitted on every run, not just failures: a scout that exits 0
                // having achieved nothing looks identical to one that did
                // nothing, unless the kill count is on the record either way.
                if let Some(summary) = outcome.memory_summary() {
                    info!(%summary, "VM memory");
                    progress(&tx, format!("scout-supervisor: VM memory: {summary}")).await;
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
        ScoutEvent::Progress {
            stream: LogStream::Stderr,
            line,
        },
    )
    .await;
}

/// Run the agent process once, streaming its output.
///
/// The [`ResultWatcher`] rides the *same* stdout loop that forwards `Progress`
/// lines, so what it classifies is byte-for-byte what was reported and the two
/// cannot disagree.
async fn spawn_attempt(
    argv: &[String],
    workdir: &Path,
    input: &str,
    before: Option<MemorySample>,
    tx: &mpsc::Sender<TaskVmEvent>,
) -> Result<(AgentOutcome, ResultWatcher)> {
    let (prog, args) = argv.split_first().context("SCOUT_AGENT_CMD is empty")?;
    info!(?prog, ?args, workdir = %workdir.display(), "running agent");

    let mut child = Command::new(prog)
        .args(args)
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
                ScoutEvent::Progress {
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
                ScoutEvent::Progress {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The template as the prompt hands it out, filled in.
    fn complete_spec() -> String {
        "## Spec: A thing\n\n\
         ### Summary\n\
         It does the thing.\n\n\
         ### Implementation Approach\n\
         - Edited `src/lib.rs`\n\n\
         ### Discovered Pitfalls\n\
         - The clock is a lie\n\n\
         ### Blockers & Dependencies\n\
         None.\n\n\
         ### Complexity\n\
         Medium\n\n\
         ### Notes\n\
         Run the tests.\n"
            .into()
    }

    /// The asymmetry that keeps this change off the healthy path: a clean
    /// exit is a spec, whatever the file looks like.
    #[test]
    fn a_clean_exit_is_always_a_spec() {
        assert_eq!(spec_verdict("## Spec: barely\n", 0), SpecVerdict::Spec);
        assert_eq!(spec_verdict("", 0), SpecVerdict::Spec);
        assert_eq!(spec_verdict(&complete_spec(), 0), SpecVerdict::Spec);
    }

    /// The existing `stub-agent.sh` fixture is the evidence the asymmetry was
    /// needed: it writes a `### Files Touched` section the template does not
    /// have and omits two it does. It exits 0, so it is never audited — but
    /// audited, it would fail.
    #[test]
    fn the_stub_fixtures_shape_would_fail_the_audit_it_never_faces() {
        let stub = "## Spec: Stub implementation\n\n\
                    ### Summary\n\
                    Stub agent ran.\n\n\
                    ### Implementation Approach\n\
                    - Added `src/stub.rs`\n\n\
                    ### Discovered Pitfalls\n\
                    - None\n\n\
                    ### Complexity\n\
                    Simple\n\n\
                    ### Files Touched\n\
                    - src/stub.rs\n";
        assert_eq!(spec_verdict(stub, 0), SpecVerdict::Spec);
        assert_eq!(
            spec_verdict(stub, 1),
            SpecVerdict::Unfinished {
                missing: vec!["Blockers & Dependencies", "Notes"]
            }
        );
    }

    #[test]
    fn a_messy_exit_with_a_complete_spec_is_still_a_spec() {
        assert_eq!(spec_verdict(&complete_spec(), 1), SpecVerdict::Spec);
        assert_eq!(spec_verdict(&complete_spec(), 137), SpecVerdict::Spec);
    }

    /// Headings are matched by keyword, so a scout that renames the sections
    /// the way real specs do is not punished for it.
    #[test]
    fn headings_match_loosely() {
        let renamed = complete_spec()
            .replace(
                "### Blockers & Dependencies",
                "### Blockers and Dependencies",
            )
            .replace(
                "### Discovered Pitfalls",
                "## Pitfalls Discovered While Exploring",
            )
            .replace("### Notes", "### Notes for the Builder");
        assert_eq!(spec_verdict(&renamed, 1), SpecVerdict::Spec);
    }

    /// An agent that wrote the shape and died leaves the template's own
    /// prose behind. Treating that as content would let a skeleton through —
    /// which is precisely the "checkpoint by writing a stub spec" failure
    /// this design exists to make impossible.
    #[test]
    fn template_placeholders_do_not_count_as_content() {
        let skeleton = "## Spec: <short title>\n\n\
                        ### Summary\n\
                        One paragraph.\n\n\
                        ### Implementation Approach\n\
                        Bullets: files changed and key design decisions.\n\n\
                        ### Discovered Pitfalls\n\
                        Edge cases, non-obvious dependencies.\n\n\
                        ### Blockers & Dependencies\n\
                        Other issues that block this.\n\n\
                        ### Complexity\n\
                        Simple | Medium | Complex\n\n\
                        ### Notes\n\
                        Anything the Builder should know.\n";
        match spec_verdict(skeleton, 1) {
            SpecVerdict::Unfinished { missing } => assert_eq!(missing.len(), 6),
            other => panic!("a pure skeleton must not be a spec: {other:?}"),
        }
    }

    /// A `#` inside a fenced block is a shell comment, not a heading — and
    /// the spec template itself is routinely quoted inside a fence.
    #[test]
    fn fenced_blocks_are_not_read_for_headings() {
        let with_fence = format!(
            "## Spec: x\n\n\
             ```sh\n\
             # Summary\n\
             echo done\n\
             ```\n\n\
             {}",
            complete_spec()
        );
        assert_eq!(spec_verdict(&with_fence, 1), SpecVerdict::Spec);

        // A fence whose headings are the *only* ones present proves the skip
        // is real rather than incidental.
        let only_fenced = "## Spec: x\n\n````markdown\n### Summary\nreal words\n\
                           ### Implementation Approach\nreal\n### Discovered Pitfalls\nreal\n\
                           ### Blockers & Dependencies\nreal\n### Complexity\nSimple\n\
                           ### Notes\nreal\n````\n";
        match spec_verdict(only_fenced, 1) {
            SpecVerdict::Unfinished { missing } => assert_eq!(missing.len(), 6),
            other => panic!("fenced headings must not count: {other:?}"),
        }
    }

    #[test]
    fn salvage_labels_each_half_and_is_none_when_empty() {
        assert!(render_salvage(None, None).is_none());
        assert!(render_salvage(Some("   "), Some("\n")).is_none());

        let both =
            render_salvage(Some("# Notes\n\nfound the bug"), Some("## Spec: half\n")).unwrap();
        assert!(both.contains("NOTES.md"));
        assert!(both.contains("found the bug"));
        assert!(both.contains("Unfinished SPEC.md"));
        assert!(both.contains("## Spec: half"));
        // The label is the point: salvage must never read as a conclusion.
        assert!(both.contains("Nothing below is a spec"));

        let notes_only = render_salvage(Some("just notes"), None).unwrap();
        assert!(!notes_only.contains("Unfinished SPEC.md"));
    }

    #[test]
    fn notes_are_trimmed_head_first_on_a_char_boundary() {
        let short = "notes".to_string();
        assert_eq!(trim_notes(short.clone()), short);

        // Multi-byte chars straddling the cut must not panic or corrupt.
        let long = format!("HEAD-MARKER{}", "é".repeat(MAX_NOTES_BYTES));
        let out = trim_notes(long);
        assert!(out.starts_with("HEAD-MARKER"), "the head is what survives");
        assert!(out.contains("NOTES.md truncated"));
        assert!(out.len() < MAX_NOTES_BYTES + 128);
    }

    #[test]
    fn the_checkpoint_interval_falls_back_on_junk() {
        // Reading process env, so this test stays single-threaded by keeping
        // every assertion on the unset/default path plus pure parsing.
        assert_eq!(
            Duration::from_secs(DEFAULT_CHECKPOINT_INTERVAL_SECS),
            Duration::from_secs(30)
        );
        assert!(checkpoint_interval() >= Duration::from_secs(1));
    }
}
