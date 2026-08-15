//! Scout dispatcher: drives the Diamond 1 loop for a single task.
//!
//! Given a Task, the dispatcher allocates a VM from vm-pool, sends a
//! [`ScoutCommand::Start`], streams back [`ScoutEvent`]s, and persists the
//! resulting [`Spec`] + queue entry to the store.
//!
//! Dispatches run concurrently: `dispatch` takes `&self`, and each call holds
//! its own event-stream subscription filtered by its VM id, so N scouts can
//! explore N tasks in parallel over one vm-pool connection (share via
//! `Arc<Scout>` or clone the underlying [`ClientHandle`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use thiserror::Error;
use tracing::{info, warn};
use vm_pool_client::{ClientError, ClientHandle, EventStream};
use vm_pool_protocol::{ServiceEvent, VmConfig, VmId};

use crate::events::EventPayload;
use crate::models::{
    Complexity, ReviewedSpec, Session, SessionId, SessionStatus, SessionUsage, Spec, SpecId,
    SpecQueueEntry, SpecQueueStatus, Task, TaskState, TranscriptStream,
};
use crate::protocol::{LogStream, ScoutCommand, ScoutEvent, TaskCommand, TaskEvent, TasksProtocol};
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub enum ScoutError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("vm-pool client: {0}")]
    Client(#[from] ClientError),
    #[error("scout failed: {0}")]
    ScoutFailed(String),
    #[error("vm-pool event stream closed before completion")]
    StreamClosed,
    /// Wall-clock deadline hit. The message lands verbatim in
    /// `sessions.exit_reason`, so both integration tests match on `timed out`.
    #[error("scout timed out after {secs}s")]
    Timeout { secs: u64 },
}

/// How this dispatcher boots a Scout VM. Uniform across dispatches — anything
/// that varies per task lives in [`ScoutTarget`].
#[derive(Debug, Clone)]
pub struct ScoutConfig {
    /// Image reference to allocate from vm-pool, e.g. `"agent:v1"`.
    pub image: String,
    /// VM configuration passed to vm-pool.
    pub vm_config: VmConfig,
    /// Wall-clock budget for one dispatch, measured from entry to
    /// [`Scout::dispatch`] so allocation is charged to it too. On expiry the
    /// VM is deallocated and the dispatch fails with [`ScoutError::Timeout`].
    pub timeout: Duration,
}

/// The repository a single dispatch explores. Per-project, so one dispatcher
/// serves every tracked project.
#[derive(Debug, Clone)]
pub struct ScoutTarget {
    /// Repo clone URL (what the scout-supervisor `git clone`s).
    pub repo_clone_url: String,
    /// Branch to base the throwaway scout branch on.
    pub base_branch: String,
}

pub struct Scout {
    store: Arc<Store>,
    client: ClientHandle<TasksProtocol>,
    config: ScoutConfig,
}

impl Scout {
    pub fn new(
        store: Arc<Store>,
        client: ClientHandle<TasksProtocol>,
        config: ScoutConfig,
    ) -> Self {
        Self {
            store,
            client,
            config,
        }
    }

    /// Dispatch a scout for `task` against `target`. Runs the full lifecycle:
    /// allocate VM, run scout, persist spec, deallocate VM.
    ///
    /// On success, returns the persisted [`Spec`]. Task state is advanced to
    /// `InReview` and a spec-queue entry is created with status
    /// `PendingReview`.
    pub async fn dispatch(&self, task: Task, target: &ScoutTarget) -> Result<Spec, ScoutError> {
        info!(task_id = %task.id, "scout dispatch starting");
        // Stamped before anything else so the deadline covers allocation too,
        // making it a true wall-clock budget rather than a drain budget.
        let started = Instant::now();

        // Subscribe before allocating so no event for our VM can be missed.
        let mut events = self.client.subscribe_events();

        // Advance task state to Scouting and log the transition.
        self.store
            .update_task_state(&task.id, TaskState::Scouting)
            .await?;
        self.store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: task.state,
                to: TaskState::Scouting,
            })
            .await?;

        let session_id = SessionId::new();

        // A re-scout after a review carries the verdict forward. This is a fact
        // about the task rather than a caller decision, so `dispatch`'s
        // signature is unchanged and the dispatch loop needs no edit.
        let prior = self.store.latest_reviewed_spec(&task.id).await?;
        if let Some(p) = &prior {
            info!(
                task_id = %task.id,
                prior_spec = %p.spec.id,
                verdict = p.status.as_str(),
                "re-scouting with the previous review"
            );
        }
        let prompt = render_prompt(&task, prior.as_ref());

        // Allocate
        let vm_id = self
            .client
            .allocate(&self.config.image, self.config.vm_config.clone())
            .await?;
        info!(%vm_id, task_id = %task.id, "allocated scout VM");

        // Persist initial session (branch filled in once Scout emits Started).
        let session_row = Session {
            id: session_id.clone(),
            task_id: task.id.clone(),
            vm_id: Some(vm_id.as_str().to_string()),
            branch: String::new(), // filled after Started
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        self.store.insert_session(&session_row).await?;
        self.store
            .append_event(EventPayload::SessionStarted {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
            })
            .await?;

        // Send Start
        if let Err(e) = self
            .client
            .send_to_vm(
                &vm_id,
                TaskCommand::Scout(ScoutCommand::Start {
                    task_id: task.id.to_string(),
                    repo_clone_url: target.repo_clone_url.clone(),
                    base_branch: target.base_branch.clone(),
                    prompt,
                }),
            )
            .await
        {
            self.finalize_failed(&session_id, &task, &vm_id, format!("send: {e}"))
                .await?;
            return Err(e.into());
        }

        // Drain events until terminal Completed / Failed, or until the budget
        // runs out. `saturating_sub` means an already-blown budget fires
        // immediately instead of wrapping.
        let (mut sink, writer) = spawn_transcript_writer(self.store.clone(), session_id.clone());
        let mut usage: Option<SessionUsage> = None;
        let remaining = self.config.timeout.saturating_sub(started.elapsed());
        let result = match tokio::time::timeout(
            remaining,
            drain_scout_events(&mut events, &vm_id, &mut sink, &mut usage),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => {
                let secs = self.config.timeout.as_secs();
                self.note_timeout(&task, &vm_id, secs).await;
                Err(ScoutError::Timeout { secs })
            }
        };

        // Close the queue and let the writer finish *before* any completion
        // event is appended, so a client refetching on that event finds the
        // whole transcript rather than a truncated one.
        if sink.dropped_total > 0 {
            warn!(
                session_id = %session_id,
                dropped = sink.dropped_total,
                "transcript lines dropped under queue pressure"
            );
        }
        drop(sink);
        if let Err(e) = writer.await {
            warn!(session_id = %session_id, error = %e, "transcript writer task failed");
        }

        // Recorded on the failure path too: a scout that burned tokens and then
        // died is the case most worth costing.
        if let Some(usage) = &usage
            && let Err(e) = self.store.update_session_usage(&session_id, usage).await
        {
            warn!(session_id = %session_id, error = %e, "recording session usage failed");
        }

        // Always try to deallocate, and never wait forever on it — the same
        // unbounded call that stalled the build queue holds a scout
        // concurrency slot here. Failures are not the dispatch's problem: the
        // pool's health loop reaps if we die mid-call. For a timeout this
        // *is* the cancel — there is deliberately no in-band Cancel command,
        // so freeing the slot means destroying the VM.
        crate::teardown::deallocate_bounded(
            &self.client,
            &self.store,
            &vm_id,
            &format!("scout for task {}", task.id),
            crate::teardown::DEALLOCATE_TIMEOUT,
        )
        .await;

        match result {
            Ok(DrainOutcome {
                branch,
                spec_markdown,
                files_touched,
                exit_code,
            }) => {
                self.finalize_succeeded(
                    &session_id,
                    &task,
                    branch,
                    spec_markdown,
                    files_touched,
                    exit_code,
                )
                .await
            }
            Err(e) => {
                let reason = format!("{e}");
                self.finalize_failed(&session_id, &task, &vm_id, reason)
                    .await?;
                Err(e)
            }
        }
    }

    /// Breadcrumb naming the deadline, written at expiry so the vm id and the
    /// budget land in the entry. Best-effort on purpose: a failed breadcrumb
    /// must not skip the deallocation that is the whole point of the timeout.
    async fn note_timeout(&self, task: &Task, vm_id: &VmId, secs: u64) {
        warn!(task_id = %task.id, %vm_id, timeout_secs = secs, "scout timed out");
        if let Err(e) = self
            .store
            .append_event(EventPayload::Note {
                source: crate::run::DISPATCHER.into(),
                message: format!(
                    "scout for {} timed out after {secs}s; deallocating {vm_id}",
                    task.id
                ),
            })
            .await
        {
            warn!(task_id = %task.id, error = %e, "could not record the timeout note");
        }
    }

    async fn finalize_succeeded(
        &self,
        session_id: &SessionId,
        task: &Task,
        branch: String,
        spec_markdown: String,
        files_touched: Vec<String>,
        _exit_code: Option<i32>,
    ) -> Result<Spec, ScoutError> {
        let now = Utc::now();
        self.store
            .update_session_branch(session_id, &branch)
            .await?;
        self.store
            .update_session_completion(session_id, SessionStatus::ScoutSucceeded, now, None)
            .await?;

        // Persist spec + queue entry. Complexity comes from the Scout's own
        // `### Complexity` section; file count is only the fallback.
        let complexity =
            parse_complexity(&spec_markdown).unwrap_or_else(|| infer_complexity(&files_touched));
        let spec = Spec {
            id: SpecId::new(),
            session_id: session_id.clone(),
            task_id: task.id.clone(),
            content: spec_markdown,
            complexity,
            files_touched,
            created_at: now,
        };
        self.store.insert_spec(&spec).await?;

        let queue = SpecQueueEntry {
            spec_id: spec.id.clone(),
            status: SpecQueueStatus::PendingReview,
            rank: None,
            approved_at: None,
            feedback: None,
            blocking_dependencies: vec![],
        };
        self.store.upsert_spec_queue_entry(&queue).await?;

        self.store
            .update_task_state(&task.id, TaskState::InReview)
            .await?;
        // A spec proves the task is dispatchable, so its past failures stop
        // counting: a later `needs_revision` re-scout starts from zero strikes.
        self.store.reset_dispatch_attempts(&task.id).await?;

        self.store
            .append_event(EventPayload::SessionCompleted {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
                status: SessionStatus::ScoutSucceeded,
            })
            .await?;
        self.store
            .append_event(EventPayload::SpecCreated {
                spec_id: spec.id.clone(),
                task_id: task.id.clone(),
                session_id: session_id.clone(),
            })
            .await?;
        self.store
            .append_event(EventPayload::SpecQueueStatusChanged {
                spec_id: spec.id.clone(),
                from: None,
                to: SpecQueueStatus::PendingReview,
                // A spec arriving is not a verdict on it.
                actor: None,
                decision_seq: None,
            })
            .await?;
        self.store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: TaskState::Scouting,
                to: TaskState::InReview,
            })
            .await?;

        Ok(spec)
    }

    async fn finalize_failed(
        &self,
        session_id: &SessionId,
        task: &Task,
        vm_id: &VmId,
        reason: String,
    ) -> Result<(), ScoutError> {
        let now = Utc::now();
        self.store
            .update_session_completion(
                session_id,
                SessionStatus::ScoutFailed,
                now,
                Some(reason.clone()),
            )
            .await?;

        // Back to Queued — a failure doesn't un-pick the work; the dispatcher
        // retries it in queue order, up to the attempt cap.
        self.store
            .update_task_state(&task.id, TaskState::Queued)
            .await?;

        self.store
            .append_event(EventPayload::SessionCompleted {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
                status: SessionStatus::ScoutFailed,
            })
            .await?;
        self.store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: TaskState::Scouting,
                to: TaskState::Queued,
            })
            .await?;
        warn!(task_id = %task.id, %vm_id, reason, "scout failed");
        Ok(())
    }
}

struct DrainOutcome {
    branch: String,
    spec_markdown: String,
    files_touched: Vec<String>,
    exit_code: Option<i32>,
}

/// Longest single line we persist. One tool result can be enormous; past this
/// the line is cut on a char boundary and marked.
const MAX_TRANSCRIPT_LINE_BYTES: usize = 32 * 1024;
/// Total transcript bytes persisted for one session. Past this we write one
/// notice and stop recording — the scout itself keeps running untouched.
const MAX_TRANSCRIPT_BYTES_PER_SESSION: usize = 8 * 1024 * 1024;
/// Depth of the hand-off queue between the drain loop and the writer task.
const TRANSCRIPT_QUEUE_CAPACITY: usize = 1024;
/// Lines coalesced into one transaction.
const TRANSCRIPT_BATCH: usize = 64;

/// The wire enum meets the domain enum here, at the dispatcher boundary, so
/// neither `models` nor `store` has to know about the protocol crate. A free
/// function rather than `From`: both enums live in other crates (tasks-api,
/// tasks-protocol), so the orphan rule forbids the impl here — and neither of
/// those crates may know about the other.
fn transcript_stream(s: LogStream) -> TranscriptStream {
    match s {
        LogStream::Stdout => TranscriptStream::Stdout,
        LogStream::Stderr => TranscriptStream::Stderr,
    }
}

/// Non-blocking handle the drain loop pushes agent output into.
///
/// `push` must never await the store: the drain loop is also what waits for
/// `Completed`, and it reads a vm-pool broadcast that drops the oldest events
/// for slow consumers. Making SQLite latency into lost scout events would be a
/// bad trade, so this is a `try_send` onto a bounded queue and a separate task
/// does the writing.
struct TranscriptSink {
    tx: tokio::sync::mpsc::Sender<(TranscriptStream, String)>,
    /// Bytes accepted so far, against `MAX_TRANSCRIPT_BYTES_PER_SESSION`.
    bytes: usize,
    /// Set once the byte budget is spent, so the notice is written once.
    capped: bool,
    /// Lines lost to queue pressure since the last marker.
    dropped: u64,
    /// Total dropped, for the summary line on the way out.
    dropped_total: u64,
}

impl TranscriptSink {
    fn push(&mut self, stream: TranscriptStream, line: String) {
        if self.capped {
            return;
        }
        let line = truncate_line(line);
        if self.bytes + line.len() > MAX_TRANSCRIPT_BYTES_PER_SESSION {
            self.capped = true;
            // Best-effort: if even this doesn't fit the queue, the log still
            // records the cap when the sink is dropped.
            let _ = self.tx.try_send((
                TranscriptStream::Stderr,
                format!(
                    "[tasks] transcript truncated: session passed {} bytes; \
                     nothing further will be recorded (the scout is unaffected)",
                    MAX_TRANSCRIPT_BYTES_PER_SESSION
                ),
            ));
            return;
        }

        // A dropped line leaves no hole a reader could detect, because seq is
        // assigned at persist time. So say so explicitly as soon as there's room.
        if self.dropped > 0
            && self
                .tx
                .try_send((
                    TranscriptStream::Stderr,
                    format!("[tasks] {} transcript line(s) dropped here", self.dropped),
                ))
                .is_ok()
        {
            self.dropped = 0;
        }

        let len = line.len();
        match self.tx.try_send((stream, line)) {
            Ok(()) => self.bytes += len,
            Err(_) => {
                self.dropped += 1;
                self.dropped_total += 1;
            }
        }
    }
}

/// Cut an over-long line on a char boundary and say how much went missing.
///
/// The `[tasks: truncated ` prefix is a cross-language contract: a cut
/// stream-json record is no longer valid JSON, and the app
/// (`app/Tasks/TranscriptView.swift`) matches this prefix to label the line
/// "truncated record" rather than dumping a wall of escaped JSON. The wording
/// after the prefix can change; the prefix can't.
fn truncate_line(line: String) -> String {
    if line.len() <= MAX_TRANSCRIPT_LINE_BYTES {
        return line;
    }
    let mut cut = MAX_TRANSCRIPT_LINE_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = line.len() - cut;
    let mut out = line;
    out.truncate(cut);
    out.push_str(&format!("…[tasks: truncated {dropped} bytes]"));
    out
}

/// Spawn the task that drains the sink's queue into the store, coalescing up
/// to [`TRANSCRIPT_BATCH`] lines per transaction. Finishes when the sink is
/// dropped and the queue is empty.
fn spawn_transcript_writer(
    store: Arc<Store>,
    session_id: SessionId,
) -> (TranscriptSink, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(TRANSCRIPT_QUEUE_CAPACITY);
    let handle = tokio::spawn(async move {
        let mut batch = Vec::with_capacity(TRANSCRIPT_BATCH);
        while rx.recv_many(&mut batch, TRANSCRIPT_BATCH).await > 0 {
            if let Err(e) = store.append_transcript_lines(&session_id, &batch).await {
                warn!(session_id = %session_id, error = %e, "persisting transcript lines failed");
            }
            batch.clear();
        }
    });
    (
        TranscriptSink {
            tx,
            bytes: 0,
            capped: false,
            dropped: 0,
            dropped_total: 0,
        },
        handle,
    )
}

/// Pull token usage out of the final stream-json `result` record.
///
/// Read field by field out of a `Value` rather than deserialized into a struct:
/// the record's shape belongs to Claude Code, and a renamed key must cost us a
/// null, not a failed scout. Key order is not assumed — real records don't put
/// `type` first.
fn parse_usage(line: &str) -> Option<SessionUsage> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "result" {
        return None;
    }
    let usage = value.get("usage");
    let u64_at =
        |obj: Option<&serde_json::Value>, key: &str| -> Option<u64> { obj?.get(key)?.as_u64() };
    Some(SessionUsage {
        input_tokens: u64_at(usage, "input_tokens"),
        output_tokens: u64_at(usage, "output_tokens"),
        cache_read_input_tokens: u64_at(usage, "cache_read_input_tokens"),
        cache_creation_input_tokens: u64_at(usage, "cache_creation_input_tokens"),
        total_cost_usd: value.get("total_cost_usd").and_then(|v| v.as_f64()),
        duration_ms: value.get("duration_ms").and_then(|v| v.as_u64()),
        num_turns: value.get("num_turns").and_then(|v| v.as_u64()),
    })
}

/// Consume this dispatch's own event subscription until its VM reports a
/// terminal Completed/Failed. Events from other VMs (concurrent scouts) are
/// ignored; service errors for our requests surface as [`ClientError`] on the
/// calls themselves, not on this stream.
async fn drain_scout_events(
    events: &mut EventStream<TasksProtocol>,
    target_vm: &VmId,
    sink: &mut TranscriptSink,
    usage_out: &mut Option<SessionUsage>,
) -> Result<DrainOutcome, ScoutError> {
    let mut branch: Option<String> = None;
    let mut exit_code: Option<i32> = None;
    loop {
        let event = events.recv().await.ok_or(ScoutError::StreamClosed)?;

        match event {
            ServiceEvent::VmApp {
                vm_id,
                event: TaskEvent::Scout(app),
            } if &vm_id == target_vm => match app {
                ScoutEvent::Started { branch: b } => {
                    branch = Some(b);
                }
                ScoutEvent::Progress { stream, line } => {
                    // The result record is the last thing the agent prints; it
                    // is also just another stdout line, so it gets persisted
                    // like the rest and parsed on the way past.
                    if stream == LogStream::Stdout
                        && let Some(usage) = parse_usage(&line)
                    {
                        *usage_out = Some(usage);
                    }
                    sink.push(transcript_stream(stream), line);
                }
                ScoutEvent::ImplementationFinished { exit_code: c } => {
                    exit_code = Some(c);
                }
                ScoutEvent::Completed {
                    spec_markdown,
                    files_touched,
                } => {
                    return Ok(DrainOutcome {
                        branch: branch.unwrap_or_default(),
                        spec_markdown,
                        files_touched,
                        exit_code,
                    });
                }
                ScoutEvent::Failed { reason } => {
                    return Err(ScoutError::ScoutFailed(reason));
                }
            },
            _other => {
                // Another VM's events, or pool-level chatter — not ours.
            }
        }
    }
}

/// Build the scout prompt, splicing in the previous attempt when the task has
/// one. The section sits between the issue body and the instructions so the
/// model reads issue → what went wrong last time → what to do.
fn render_prompt(task: &Task, prior: Option<&ReviewedSpec>) -> String {
    let previous = prior.map(render_previous_attempt).unwrap_or_default();
    format!(
        "You are a Scout in the Double Diamond architecture.\n\n\
         ## Issue: {title} (#{num})\n\n\
         {body}\n\n\
         {previous}\
         ## Your job\n\n\
         1. Implement a working solution in the cloned repo (cwd).\n\
         2. Run the project's tests / lint / typecheck — get them green.\n\
         3. Write `SPEC.md` in the repo root with the structure below.\n\
         4. Do NOT create a PR or push anywhere.\n\n\
         ## SPEC.md structure\n\n\
         ```\n\
         ## Spec: <short title>\n\n\
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
         Anything the Builder should know.\n\
         ```\n",
        title = task.title,
        num = task.gh_issue_number,
        body = task.body,
        previous = previous,
    )
}

/// Render the `## Previous attempt` section: the verdict, the reviewer's
/// feedback verbatim, and the spec it was written about. The spec is quoted
/// rather than summarised because feedback like "section 3 is underspecified"
/// is meaningless without section 3.
fn render_previous_attempt(prior: &ReviewedSpec) -> String {
    let feedback = prior
        .feedback
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or("(no written feedback was left)");
    let fence = fence_for(&prior.spec.content);
    format!(
        "## Previous attempt\n\n\
         A previous scout explored this same issue and produced the spec below. \
         A reviewer read it and returned a verdict of **{verdict}**.\n\n\
         Treat the reviewer's feedback as a requirement, not a suggestion. Do not \
         resubmit the previous spec unchanged, and do not repeat the shortcomings \
         it identifies. Explore the current code yourself — the previous spec may \
         itself be out of date.\n\n\
         ### Reviewer feedback\n\n\
         {feedback}\n\n\
         ### Previous spec\n\n\
         {fence}markdown\n\
         {spec}\n\
         {fence}\n\n",
        verdict = prior.status.as_str(),
        feedback = feedback,
        fence = fence,
        spec = prior.spec.content.trim_end(),
    )
}

/// A fence long enough to quote `content` intact: one backtick longer than the
/// longest backtick run inside it, minimum three. Specs routinely contain
/// fenced code, and a plain ``` wrapper would be closed by the first one.
fn fence_for(content: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in content.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

/// The Scout's self-reported complexity: the first non-empty line after a
/// `### Complexity` heading. Returns `None` when the section is missing or
/// names zero or several levels (e.g. a lazily-copied `Simple | Medium |
/// Complex` template line).
fn parse_complexity(spec: &str) -> Option<Complexity> {
    let mut in_section = false;
    for line in spec.lines() {
        let t = line.trim();
        if in_section {
            if t.is_empty() {
                continue;
            }
            if t.starts_with('#') {
                return None;
            }
            let lower = t.to_lowercase();
            let matched: Vec<Complexity> = [
                (lower.contains("simple"), Complexity::Simple),
                (lower.contains("medium"), Complexity::Medium),
                (lower.contains("complex"), Complexity::Complex),
            ]
            .into_iter()
            .filter_map(|(hit, c)| hit.then_some(c))
            .collect();
            return match matched.as_slice() {
                [one] => Some(*one),
                _ => None,
            };
        }
        if t.eq_ignore_ascii_case("### complexity") {
            in_section = true;
        }
    }
    None
}

fn infer_complexity(files_touched: &[String]) -> Complexity {
    match files_touched.len() {
        0..=2 => Complexity::Simple,
        3..=8 => Complexity::Medium,
        _ => Complexity::Complex,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_fixture() -> Task {
        Task {
            id: crate::models::TaskId::new(),
            project_id: crate::models::ProjectId::new(),
            gh_issue_number: 42,
            title: "A title".into(),
            body: "The issue body.".into(),
            labels: vec![],
            gh_state: crate::models::GhState::Open,
            state: TaskState::Backlog,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn reviewed(content: &str, feedback: Option<&str>) -> ReviewedSpec {
        ReviewedSpec {
            spec: Spec {
                id: SpecId::new(),
                session_id: SessionId::new(),
                task_id: crate::models::TaskId::new(),
                content: content.into(),
                complexity: Complexity::Simple,
                files_touched: vec![],
                created_at: Utc::now(),
            },
            status: SpecQueueStatus::NeedsRevision,
            feedback: feedback.map(Into::into),
        }
    }

    #[test]
    fn a_fresh_prompt_has_no_previous_attempt_section() {
        let prompt = render_prompt(&task_fixture(), None);
        assert!(!prompt.contains("Previous attempt"));
        // The body must still run straight into the instructions.
        assert!(prompt.contains("The issue body.\n\n## Your job"));
    }

    #[test]
    fn a_re_scout_prompt_carries_the_verdict_feedback_and_prior_spec() {
        let prior = reviewed(
            "## Spec: old\n\nSection 3 is thin.",
            Some("Flesh out section 3."),
        );
        let prompt = render_prompt(&task_fixture(), Some(&prior));

        let attempt = prompt.find("## Previous attempt").expect("section present");
        let verdict = prompt.find("needs_revision").expect("verdict present");
        let feedback = prompt
            .find("Flesh out section 3.")
            .expect("feedback present");
        let spec = prompt
            .find("Section 3 is thin.")
            .expect("prior spec present");
        let job = prompt.find("## Your job").expect("instructions present");

        // Order matters: issue → verdict → feedback → prior spec → instructions.
        assert!(attempt < verdict && verdict < feedback && feedback < spec && spec < job);
    }

    #[test]
    fn missing_or_blank_feedback_still_renders() {
        for empty in [None, Some(""), Some("   ")] {
            let prompt = render_prompt(&task_fixture(), Some(&reviewed("spec body", empty)));
            assert!(prompt.contains("## Previous attempt"));
            assert!(prompt.contains("no written feedback"));
        }
    }

    #[test]
    fn the_fence_outlives_fences_nested_in_the_quoted_spec() {
        // A spec containing its own ```rust block would break out of a plain
        // ``` wrapper and merge its headings into the prompt's structure.
        let nested = "## Spec\n\n```rust\nfn x() {}\n```\n";
        let prompt = render_prompt(&task_fixture(), Some(&reviewed(nested, Some("f"))));
        assert!(prompt.contains("````markdown"));
        assert_eq!(fence_for("no fences"), "```");
        assert_eq!(fence_for("a ``` b"), "````");
        assert_eq!(fence_for("a ````` b"), "``````");
    }

    #[test]
    fn usage_parses_regardless_of_key_order_and_survives_junk() {
        let record = r#"{"subtype":"success","duration_ms":1234,"num_turns":3,
            "total_cost_usd":0.0421,"usage":{"input_tokens":1200,"output_tokens":340},
            "type":"result"}"#;
        let usage = parse_usage(record).expect("parses with type last");
        assert_eq!(usage.input_tokens, Some(1200));
        assert_eq!(usage.output_tokens, Some(340));
        assert_eq!(usage.total_cost_usd, Some(0.0421));
        assert_eq!(usage.num_turns, Some(3));
        // Absent keys are nulls, not failures.
        assert_eq!(usage.cache_read_input_tokens, None);

        assert!(parse_usage(r#"{"type":"assistant"}"#).is_none());
        assert!(parse_usage("not json at all").is_none());
        assert!(parse_usage(r#"{"type":"result"}"#).is_some());
    }

    #[test]
    fn over_long_lines_are_cut_on_a_char_boundary_and_marked() {
        // Multi-byte chars straddling the cut must not panic or corrupt.
        let line = "é".repeat(MAX_TRANSCRIPT_LINE_BYTES);
        let out = truncate_line(line);
        assert!(out.contains("[tasks: truncated"));
        assert!(out.len() < MAX_TRANSCRIPT_LINE_BYTES + 64);

        let short = "left alone".to_string();
        assert_eq!(truncate_line(short.clone()), short);
    }

    #[test]
    fn a_cut_stream_json_record_stops_being_json_but_stays_marked() {
        // Why the app needs the marker: one `Read` of a moderately large file
        // is enough to blow the per-line cap, and what's left is no longer a
        // parseable record — only the marker says why.
        let record = serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "content": "y".repeat(MAX_TRANSCRIPT_LINE_BYTES),
                }],
            },
        })
        .to_string();
        assert!(serde_json::from_str::<serde_json::Value>(&record).is_ok());

        let cut = truncate_line(record);
        assert!(
            serde_json::from_str::<serde_json::Value>(&cut).is_err(),
            "a cut record must stop parsing — that's what puts it in the app's raw path"
        );
        assert!(cut.contains("[tasks: truncated "));
    }

    #[test]
    fn the_session_byte_cap_counts_bytes_not_lines() {
        // Room for every push, so nothing is lost to queue pressure and the
        // cap is the only thing that can stop recording.
        let (tx, mut rx) = tokio::sync::mpsc::channel(TRANSCRIPT_QUEUE_CAPACITY);
        let mut sink = TranscriptSink {
            tx,
            bytes: 0,
            capped: false,
            dropped: 0,
            dropped_total: 0,
        };

        let line = "x".repeat(MAX_TRANSCRIPT_LINE_BYTES);
        let fits = MAX_TRANSCRIPT_BYTES_PER_SESSION / MAX_TRANSCRIPT_LINE_BYTES;
        for _ in 0..fits {
            sink.push(TranscriptStream::Stdout, line.clone());
        }
        assert!(!sink.capped, "exactly the budget must not trip the cap");

        // One byte over the budget trips it and queues the notice.
        sink.push(TranscriptStream::Stdout, "x".into());
        assert!(sink.capped);
        sink.push(TranscriptStream::Stdout, "ignored after the cap".into());

        let mut recorded = 0;
        let mut last = String::new();
        while let Ok((_, l)) = rx.try_recv() {
            recorded += 1;
            last = l;
        }
        assert_eq!(recorded, fits + 1, "capped pushes must not be recorded");
        assert!(last.contains("transcript truncated"));
    }

    #[test]
    fn parse_complexity_reads_section() {
        let spec = "## Spec\n\n### Complexity\n\nMedium\n\n### Notes\n";
        assert_eq!(parse_complexity(spec), Some(Complexity::Medium));
    }

    #[test]
    fn parse_complexity_rejects_template_line_and_missing_section() {
        let template = "### Complexity\nSimple | Medium | Complex\n";
        assert_eq!(parse_complexity(template), None);
        assert_eq!(parse_complexity("## Spec\nno section"), None);
        let empty_section = "### Complexity\n\n### Notes\nx";
        assert_eq!(parse_complexity(empty_section), None);
    }
}
