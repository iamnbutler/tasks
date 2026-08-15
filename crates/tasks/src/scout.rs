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
    Complexity, ReviewedSpec, ScoutNotes, Session, SessionId, SessionStatus, SessionUsage, Spec,
    SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskState, TranscriptOwner,
};
use crate::protocol::{LogStream, ScoutCommand, ScoutEvent, TaskCommand, TaskEvent, TasksProtocol};
use crate::store::{Store, StoreError};
use crate::transcript::{TranscriptSink, spawn_transcript_writer, transcript_stream};

#[derive(Debug, Error)]
pub enum ScoutError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("vm-pool client: {0}")]
    Client(#[from] ClientError),
    #[error("scout failed: {0}")]
    ScoutFailed(String),
    /// The run ended without a spec but left notes behind. Still an error —
    /// so [`crate::run::record_outcome`] ticks the attempt count and a scout
    /// that dies at the same point every time cannot retry forever — but a
    /// distinguishable one, because the salvage changes what the next attempt
    /// is given.
    #[error("scout stopped early: {0}")]
    StoppedEarly(String),
    #[error("vm-pool event stream closed before completion")]
    StreamClosed,
    /// Wall-clock deadline hit. The message lands verbatim in
    /// `sessions.exit_reason`, so the timeout integration tests match on
    /// `timed out` — including the one where the run was salvaged.
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
        // Salvage from an interrupted attempt, if this task has any. Its only
        // consumer is right here — quoted into the prompt as an unverified
        // lead, never as a spec.
        let salvage = self.store.salvage_for_task(&task.id).await?;
        if let Some(notes) = &salvage {
            info!(
                task_id = %task.id,
                from_session = %notes.session_id,
                bytes = notes.notes.len(),
                "carrying field notes from an interrupted attempt"
            );
        }
        let prompt = render_prompt(&task, prior.as_ref(), salvage.as_ref());

        // Allocate
        let vm_id = self
            .client
            .allocate(&self.config.image, self.config.vm_config.clone())
            .await?;
        // The VM's shape is logged with the allocation so a failure that only
        // makes sense in terms of memory (an OOM-killed linker) can be
        // correlated with what this VM actually had.
        info!(
            %vm_id,
            task_id = %task.id,
            cpus = ?self.config.vm_config.cpus,
            memory_mb = ?self.config.vm_config.memory_mb,
            "allocated scout VM"
        );

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
        // #849 gave the transcript writer an owner; #856 added the checkpoint
        // writer beside it. Both survive: the sink is owner-addressed, and the
        // notes it salvages are still keyed to this session and task.
        let (mut sink, writer) =
            spawn_transcript_writer(self.store.clone(), TranscriptOwner::session(&session_id));
        let (mut checkpoints, checkpoint_writer) =
            spawn_checkpoint_writer(self.store.clone(), session_id.clone(), task.id.clone());
        // Everything the drain loop learns lives out here, not inside the
        // future: `tokio::time::timeout` *drops* that future at the deadline,
        // and the deadline is the case this whole feature exists for. State
        // held inside it would be destroyed exactly when it is needed.
        let mut state = DrainState::default();
        let remaining = self.config.timeout.saturating_sub(started.elapsed());
        let result = match tokio::time::timeout(
            remaining,
            drain_scout_events(&mut events, &vm_id, &mut sink, &mut checkpoints, &mut state),
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
        crate::transcript::flush(sink, writer, session_id.as_str()).await;

        // Joined here, before any finalize: a checkpoint still in the queue
        // would land *after* the final salvage and overwrite it with an older,
        // shorter version of the same notes.
        if checkpoints.dropped > 0 {
            warn!(
                session_id = %session_id,
                dropped = checkpoints.dropped,
                "checkpoints dropped under queue pressure; the newest one still lands"
            );
        }
        drop(checkpoints);
        if let Err(e) = checkpoint_writer.await {
            warn!(session_id = %session_id, error = %e, "checkpoint writer task failed");
        }

        // Recorded on the failure path too: a scout that burned tokens and then
        // died is the case most worth costing.
        if let Some(usage) = &state.usage
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

        let branch = state.branch.clone().unwrap_or_default();
        match result {
            Ok(DrainOutcome::Concluded {
                spec_markdown,
                files_touched,
            }) => {
                self.finalize_succeeded(
                    &session_id,
                    &task,
                    branch,
                    spec_markdown,
                    files_touched,
                    state.exit_code,
                )
                .await
            }
            // The supervisor said so itself: the run ended without concluding.
            Ok(DrainOutcome::StoppedEarly {
                reason,
                notes_markdown,
                files_touched,
            }) => {
                self.finalize_stopped_early(
                    &session_id,
                    &task,
                    branch,
                    &reason,
                    notes_markdown,
                    files_touched,
                )
                .await?;
                Err(ScoutError::StoppedEarly(reason))
            }
            Err(e) => {
                let reason = format!("{e}");
                // The deadline and a dead stream never reach the supervisor's
                // own reporting — the VM is destroyed where it stands. The
                // last checkpoint is all there is, and it is worth the same
                // as one the supervisor handed over deliberately.
                //
                // The error is returned unchanged either way. `Timeout` in
                // particular keeps its shape: CLAUDE.md and the timeout
                // integration tests pin `exit_reason` containing "timed out",
                // and a salvaged timeout is still a timeout.
                match state.checkpoint.take() {
                    Some(notes) => {
                        self.finalize_stopped_early(
                            &session_id,
                            &task,
                            branch,
                            &reason,
                            notes,
                            Vec::new(),
                        )
                        .await?
                    }
                    None => {
                        self.finalize_failed(&session_id, &task, &vm_id, reason)
                            .await?
                    }
                }
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

    /// Everything the failure path does, plus the salvage — and nothing the
    /// success path does.
    ///
    /// No [`Spec`] row, no queue entry, no state that any reviewer can reach:
    /// the notes exist only for the next attempt's prompt. The task goes back
    /// to `Queued` and the attempt still counts, exactly as for a failure,
    /// because a scout that stops early at the same point every time must not
    /// retry forever.
    async fn finalize_stopped_early(
        &self,
        session_id: &SessionId,
        task: &Task,
        branch: String,
        reason: &str,
        notes: String,
        files_touched: Vec<String>,
    ) -> Result<(), ScoutError> {
        let now = Utc::now();
        if !branch.is_empty() {
            self.store
                .update_session_branch(session_id, &branch)
                .await?;
        }

        // Notes before the status flip: a crash in between leaves a `running`
        // session that has notes, which is exactly what the orphan sweep reads
        // to mark it `scout_stopped_early` rather than `scout_failed`.
        self.store
            .upsert_scout_notes(&ScoutNotes {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
                reason: Some(reason.to_string()),
                notes,
                files_touched,
                updated_at: now,
            })
            .await?;
        self.store
            .update_session_completion(
                session_id,
                SessionStatus::ScoutStoppedEarly,
                now,
                Some(reason.to_string()),
            )
            .await?;
        self.store
            .update_task_state(&task.id, TaskState::Queued)
            .await?;

        self.store
            .append_event(EventPayload::SessionCompleted {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
                status: SessionStatus::ScoutStoppedEarly,
            })
            .await?;
        self.store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: TaskState::Scouting,
                to: TaskState::Queued,
            })
            .await?;
        warn!(task_id = %task.id, session_id = %session_id, reason, "scout stopped early; notes salvaged");
        Ok(())
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

/// How a drained scout run ended, when it ended on its own terms.
///
/// Two outcomes rather than one plus an error, because "stopped early" is
/// neither: it produced no spec, and it is not the same fact as a run that
/// produced nothing at all.
enum DrainOutcome {
    /// The scout concluded. This is the only path to a [`Spec`].
    Concluded {
        spec_markdown: String,
        files_touched: Vec<String>,
    },
    /// The run ended without a spec, but with something written down.
    StoppedEarly {
        reason: String,
        notes_markdown: String,
        files_touched: Vec<String>,
    },
}

/// What the drain loop has learned, kept by the caller rather than the drain
/// future so that dropping the future (which is how the deadline cancels)
/// does not take it with it.
#[derive(Default)]
struct DrainState {
    branch: Option<String>,
    exit_code: Option<i32>,
    usage: Option<SessionUsage>,
    /// The most recent checkpoint. On the timeout path this is the entire
    /// salvage — the VM is destroyed, and the supervisor never gets to say
    /// anything more.
    checkpoint: Option<String>,
}

/// Depth of the checkpoint hand-off queue. Small on purpose: checkpoints
/// arrive tens of seconds apart, each one supersedes the last, and the final
/// salvage is written by `finalize_stopped_early` with an awaited call. A drop
/// here costs resolution during a crash window, never the salvage itself.
const CHECKPOINT_QUEUE_CAPACITY: usize = 4;

/// Non-blocking handle for persisting checkpoints as they arrive.
///
/// `push` never awaits the store, for the same reason
/// [`TranscriptSink::push`] doesn't: the drain loop is also what waits for the
/// terminal event, and it reads a vm-pool broadcast that drops the oldest
/// events for slow consumers. SQLite latency must not cost scout events.
struct CheckpointSink {
    tx: tokio::sync::mpsc::Sender<String>,
    dropped: u64,
}

impl CheckpointSink {
    fn push(&mut self, notes: String) {
        if self.tx.try_send(notes).is_err() {
            self.dropped += 1;
        }
    }
}

/// Spawn the task that persists checkpoints off the drain loop.
///
/// Persisted on arrival rather than at the end because the two deaths this
/// insures against — a VM deallocated at the deadline and a server restart —
/// never reach an end-of-run path. Notes held only in memory would die with
/// the process, and two of #825's four failures were exactly that.
fn spawn_checkpoint_writer(
    store: Arc<Store>,
    session_id: SessionId,
    task_id: crate::models::TaskId,
) -> (CheckpointSink, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(CHECKPOINT_QUEUE_CAPACITY);
    let handle = tokio::spawn(async move {
        while let Some(notes) = rx.recv().await {
            let row = ScoutNotes {
                session_id: session_id.clone(),
                task_id: task_id.clone(),
                // No reason yet: a checkpoint is written before anyone knows
                // how the run ends.
                reason: None,
                notes,
                files_touched: Vec::new(),
                updated_at: Utc::now(),
            };
            if let Err(e) = store.upsert_scout_notes(&row).await {
                warn!(session_id = %session_id, error = %e, "persisting a scout checkpoint failed");
            }
        }
    });
    (CheckpointSink { tx, dropped: 0 }, handle)
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
    checkpoints: &mut CheckpointSink,
    state: &mut DrainState,
) -> Result<DrainOutcome, ScoutError> {
    loop {
        let event = events.recv().await.ok_or(ScoutError::StreamClosed)?;

        match event {
            ServiceEvent::VmApp {
                vm_id,
                event: TaskEvent::Scout(app),
            } if &vm_id == target_vm => match app {
                ScoutEvent::Started { branch: b } => {
                    state.branch = Some(b);
                }
                ScoutEvent::Progress { stream, line } => {
                    // The result record is the last thing the agent prints; it
                    // is also just another stdout line, so it gets persisted
                    // like the rest and parsed on the way past.
                    if stream == LogStream::Stdout
                        && let Some(usage) = parse_usage(&line)
                    {
                        state.usage = Some(usage);
                    }
                    sink.push(transcript_stream(stream), line);
                }
                // Kept twice over: persisted (so it survives this process
                // dying) and held in `state` (so it survives this future being
                // dropped at the deadline). Neither covers the other.
                ScoutEvent::Checkpoint { notes_markdown } => {
                    checkpoints.push(notes_markdown.clone());
                    state.checkpoint = Some(notes_markdown);
                }
                ScoutEvent::ImplementationFinished { exit_code: c } => {
                    state.exit_code = Some(c);
                }
                ScoutEvent::Completed {
                    spec_markdown,
                    files_touched,
                } => {
                    return Ok(DrainOutcome::Concluded {
                        spec_markdown,
                        files_touched,
                    });
                }
                ScoutEvent::StoppedEarly {
                    reason,
                    notes_markdown,
                    files_touched,
                } => {
                    return Ok(DrainOutcome::StoppedEarly {
                        reason,
                        notes_markdown,
                        files_touched,
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
fn render_prompt(
    task: &Task,
    prior: Option<&ReviewedSpec>,
    salvage: Option<&ScoutNotes>,
) -> String {
    let previous = prior.map(render_previous_attempt).unwrap_or_default();
    let field_notes = salvage.map(render_field_notes).unwrap_or_default();
    format!(
        "You are a Scout in the Double Diamond architecture.\n\n\
         ## Issue: {title} (#{num})\n\n\
         {body}\n\n\
         {previous}\
         {field_notes}\
         ## Your job\n\n\
         1. Implement a working solution in the cloned repo (cwd).\n\
         2. Keep `NOTES.md` in the repo root up to date as you go: findings, \
         dead ends, where things live, anything you would hate to re-derive. \
         It is read back every 30 seconds and is the only thing that survives \
         if this run is cut short, so write it as you learn rather than at the \
         end.\n\
         3. Run the project's tests / lint / typecheck — get them green.\n\
         4. Write `SPEC.md` in the repo root with the structure below, and \
         only once you have actually concluded. **`SPEC.md` is not a \
         checkpoint.** A half-written spec is worse than no spec, because it \
         reaches a reviewer looking finished. If you want to record progress, \
         that is what `NOTES.md` is for.\n\
         5. Do NOT create a PR or push anywhere.\n\n\
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
        field_notes = field_notes,
    )
}

/// Largest slice of salvaged notes a prompt will carry.
///
/// Much smaller than the transport's [`crate::protocol::MAX_NOTES_BYTES`], and
/// the difference is the point: 256 KiB is fine on the wire and ruinous in a
/// retry's context window. Trimmed head-first because notes are written
/// top-down — a tail-first cut would hand the next scout conclusions with
/// nothing to attach them to.
const MAX_PROMPT_NOTES_BYTES: usize = 32 * 1024;

/// Render the `## Field notes from an interrupted attempt` section.
///
/// Framed hard as unverified, because that is the whole distinction being
/// preserved: these notes never passed a review, and a scout that treats them
/// as conclusions inherits mistakes nobody checked.
fn render_field_notes(salvage: &ScoutNotes) -> String {
    let reason = salvage
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("the run was cut short before it could say why");
    let notes = trim_prompt_notes(salvage.notes.trim());
    let fence = fence_for(&notes);
    format!(
        "## Field notes from an interrupted attempt\n\n\
         An earlier scout on this issue was interrupted before it reached a \
         conclusion ({reason}). These are the notes it had written down at \
         that point.\n\n\
         **Nothing below has been verified.** It is not a spec, it was never \
         reviewed, and it may be wrong or out of date. Treat it as leads worth \
         checking first, not as findings you can rely on — confirm anything \
         you intend to use against the code in front of you.\n\n\
         {fence}markdown\n\
         {notes}\n\
         {fence}\n\n"
    )
}

/// Cut salvaged notes to [`MAX_PROMPT_NOTES_BYTES`] on a char boundary,
/// keeping the head and saying so.
fn trim_prompt_notes(notes: &str) -> String {
    if notes.len() <= MAX_PROMPT_NOTES_BYTES {
        return notes.to_string();
    }
    let mut cut = MAX_PROMPT_NOTES_BYTES;
    while cut > 0 && !notes.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = notes.len() - cut;
    format!(
        "{}\n\n…[tasks: field notes truncated here, {dropped} bytes dropped]",
        &notes[..cut]
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
        let prompt = render_prompt(&task_fixture(), None, None);
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
        let prompt = render_prompt(&task_fixture(), Some(&prior), None);

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
            let prompt = render_prompt(&task_fixture(), Some(&reviewed("spec body", empty)), None);
            assert!(prompt.contains("## Previous attempt"));
            assert!(prompt.contains("no written feedback"));
        }
    }

    #[test]
    fn the_fence_outlives_fences_nested_in_the_quoted_spec() {
        // A spec containing its own ```rust block would break out of a plain
        // ``` wrapper and merge its headings into the prompt's structure.
        let nested = "## Spec\n\n```rust\nfn x() {}\n```\n";
        let prompt = render_prompt(&task_fixture(), Some(&reviewed(nested, Some("f"))), None);
        assert!(prompt.contains("````markdown"));
        assert_eq!(fence_for("no fences"), "```");
        assert_eq!(fence_for("a ``` b"), "````");
        assert_eq!(fence_for("a ````` b"), "``````");
    }

    fn salvaged(notes: &str, reason: Option<&str>) -> ScoutNotes {
        ScoutNotes {
            session_id: SessionId::new(),
            task_id: crate::models::TaskId::new(),
            reason: reason.map(Into::into),
            notes: notes.into(),
            files_touched: vec![],
            updated_at: Utc::now(),
        }
    }

    /// The instruction that keeps "checkpoint early" from turning into "write
    /// a skeleton spec early" — which would defeat the whole design, since a
    /// skeleton spec is what reaches a reviewer looking finished.
    #[test]
    fn the_prompt_asks_for_notes_and_forbids_a_placeholder_spec() {
        let prompt = render_prompt(&task_fixture(), None, None);
        assert!(prompt.contains("Keep `NOTES.md`"));
        assert!(prompt.contains("`SPEC.md` is not a checkpoint"));
        assert!(prompt.contains("A half-written spec is worse than no spec"));
        // No interrupted attempt, so no section quoting one.
        assert!(!prompt.contains("Field notes"));
    }

    #[test]
    fn salvage_reaches_the_next_prompt_marked_unverified() {
        let notes = salvaged(
            "# Notes\n\nThe parser lives in src/parse.rs.",
            Some("scout timed out after 3600s"),
        );
        let prompt = render_prompt(&task_fixture(), None, Some(&notes));

        let section = prompt
            .find("## Field notes from an interrupted attempt")
            .expect("section present");
        let body = prompt.find("The parser lives in").expect("notes quoted");
        let job = prompt.find("## Your job").expect("instructions present");
        assert!(section < body && body < job, "issue → notes → instructions");

        // The framing is the point: unverified leads, not findings.
        assert!(prompt.contains("Nothing below has been verified."));
        assert!(prompt.contains("It is not a spec"));
        assert!(prompt.contains("scout timed out after 3600s"));

        // A checkpoint salvaged mid-run has no reason yet; the section still
        // renders rather than printing "None".
        let no_reason = render_prompt(&task_fixture(), None, Some(&salvaged("x", None)));
        assert!(no_reason.contains("cut short before it could say why"));
        assert!(!no_reason.contains("None)"));
    }

    /// Notes containing their own fenced code must not break out of the
    /// wrapper and merge into the prompt's structure — same trap as the
    /// quoted prior spec.
    #[test]
    fn quoted_notes_survive_their_own_fences() {
        let notes = salvaged("```rust\nfn x() {}\n```", None);
        let prompt = render_prompt(&task_fixture(), None, Some(&notes));
        assert!(prompt.contains("````markdown"));
    }

    /// The transport cap is 256 KiB; a prompt cap of the same size would
    /// spend a retry's context window on the thing it was meant to help.
    #[test]
    fn prompt_notes_are_trimmed_head_first() {
        let short = "still short";
        assert_eq!(trim_prompt_notes(short), short);

        let long = format!("HEAD{}TAIL", "é".repeat(MAX_PROMPT_NOTES_BYTES));
        let out = trim_prompt_notes(&long);
        assert!(out.starts_with("HEAD"), "the head is what survives");
        assert!(!out.contains("TAIL"));
        assert!(out.contains("field notes truncated"));
        assert!(out.len() < MAX_PROMPT_NOTES_BYTES + 128);
        const { assert!(MAX_PROMPT_NOTES_BYTES < crate::protocol::MAX_NOTES_BYTES) };
    }

    /// A re-scout can carry both: a reviewed spec *and* leads from a run that
    /// never got as far as a verdict. History first, in that order.
    #[test]
    fn a_prompt_can_carry_both_a_review_and_field_notes() {
        let prior = reviewed("## Spec: old", Some("Say more."));
        let notes = salvaged("a later, interrupted look", None);
        let prompt = render_prompt(&task_fixture(), Some(&prior), Some(&notes));

        let previous = prompt.find("## Previous attempt").unwrap();
        let field = prompt.find("## Field notes").unwrap();
        let job = prompt.find("## Your job").unwrap();
        assert!(previous < field && field < job);
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
