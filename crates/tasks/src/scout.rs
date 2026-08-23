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
//!
//! A dispatch is two halves: setting the run up (state, session row, VM,
//! `Start`) and then [`Scout::follow`]ing it (drain → deallocate →
//! finalize). [`Scout::reattach`] re-enters the second half for a run a
//! previous process started, which is the whole of what a restart costs now.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use thiserror::Error;
use tracing::{info, warn};
use vm_pool_client::{ClientError, ClientHandle};
use vm_pool_protocol::{VmConfig, VmId};

use crate::broker::{CloneSource, LeaseIssuer, SubjectKind};
use crate::cancel::Bounded;
use crate::deadline::{Deadline, Expiry};
use crate::events::EventPayload;
use crate::models::{
    Complexity, Directions, ReviewedSpec, RunKind, ScoutNotes, Session, SessionId, SessionStatus,
    SessionUsage, Spec, SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskState, TranscriptOwner,
    TranscriptStream,
};
use crate::protocol::{
    FailureClass, LogStream, ScoutCommand, ScoutEvent, TaskCommand, TaskEvent, TasksProtocol,
};
use crate::reattach::{AppEvents, Origin};
use crate::store::{CancelRequest, Store, StoreError};
use crate::transcript::{TranscriptSink, spawn_transcript_writer, transcript_stream};

#[derive(Debug, Error)]
pub enum ScoutError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("vm-pool client: {0}")]
    Client(#[from] ClientError),
    /// The supervisor reported a terminal failure with nothing to salvage.
    /// `class` is *its* answer to whether the run judged the work, carried on
    /// the event itself — see [`FailureClass`].
    #[error("scout failed: {reason}")]
    ScoutFailed { reason: String, class: FailureClass },
    /// The session could not be picked up after a restart: no VM recorded, the
    /// pool no longer has it, and nothing terminal in the replay. The run is
    /// lost, but — see [`crate::run::record_outcome`] — it is *not* the task's
    /// fault, so it must not burn a dispatch attempt. Reconciliation never
    /// charged one either; three restarts would otherwise reject a perfectly
    /// good task. Classified [`FailureClass::Orphaned`], which is the same
    /// question one level up.
    #[error("scout could not be resumed: {0}")]
    NotResumable(String),
    /// The run ended without a spec but left notes behind. Still an error —
    /// so [`crate::run::record_outcome`] ticks the attempt count when the run
    /// was a verdict, and a scout that dies at the same point every time
    /// cannot retry forever — but a distinguishable one, because the salvage
    /// changes what the next attempt is given.
    #[error("scout stopped early: {reason}")]
    StoppedEarly { reason: String, class: FailureClass },
    /// Somebody stopped the run on purpose. Carries the whole request, because
    /// what makes a cancel legible afterwards is the actor and the rationale —
    /// [`crate::run::record_outcome`] therefore charges no dispatch attempt,
    /// and [`Scout::finalize_cancelled`] writes them into `exit_reason`.
    #[error("scout cancelled by {}", .0.actor.as_str())]
    Cancelled(CancelRequest),
    #[error("vm-pool event stream closed before completion")]
    StreamClosed,
    /// Deadline hit with the run awake for essentially all of it — including
    /// the case where the host napped briefly and the run still spent its
    /// budget, which is #944. The message lands verbatim in
    /// `sessions.exit_reason`, so the timeout integration tests match on
    /// `timed out` — including the one where the run was salvaged.
    ///
    /// `secs` is the *configured* budget, never the expiry's: on the reattach
    /// path the effective budget is the remainder, and the integration tests
    /// pin specific numbers.
    #[error("scout timed out after {secs}s")]
    Timeout { secs: u64 },
    /// The budget ran out with enough of it *unspent* that the run was never
    /// really given its time — the machine was not running to give it. The two
    /// clocks in [`crate::deadline`] measure that, and
    /// [`Expiry::starved_by_suspend`] is where the line sits (#944: a host that
    /// merely napped is not this; a host that took a quarter of the budget
    /// away is). Nothing here judged the work, so it is
    /// [`FailureClass::Transport`] and costs the task no attempt (#929).
    #[error("scout abandoned: {0}")]
    Suspended(Expiry),
}

impl ScoutError {
    /// Whether this failure judged the work — the one decision point this
    /// dispatcher has, read by [`crate::run::record_outcome`].
    ///
    /// `StreamClosed` is `Transport`: the vm-pool event stream ending is the
    /// daemon going away — a routine maintenance action, not a judgement on
    /// the work. So is `Suspended`, which is the host having been asleep for
    /// enough of the budget that the run never had it to spend.
    ///
    /// Everything not named here is a [`FailureClass::Verdict`], and two of
    /// those are deliberate. A `Timeout` had all but a small share of the
    /// budget *awake* and still produced nothing, which is as much of a verdict
    /// as an agent that
    /// concluded empty-handed. And a `Store` or `Client` failure that reaches
    /// here is not a disconnect — `crate::run::is_disconnect` answers that
    /// separately and earlier, because it also decides whether to drop the
    /// vm-pool client, which this question does not.
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::ScoutFailed { class, .. } | Self::StoppedEarly { class, .. } => *class,
            Self::NotResumable(_) => FailureClass::Orphaned,
            Self::Cancelled(_) => FailureClass::Cancelled,
            // `crate::run::is_disconnect` reaches this error first on the
            // dispatch path and already spares it its attempt, so nothing
            // here changes what a scout is charged today. It is what makes
            // the classification honest for every *other* reader — and what
            // keeps the two answers from disagreeing about the same error.
            // The builder half is the live bug this mirrors.
            Self::StreamClosed => FailureClass::Transport,
            // #929: a nine-hour suspend that fired the deadline three minutes
            // after the lid opened, charging three specs an attempt each for a
            // budget the run was never awake to spend. Classified beside
            // `Egress` and `StreamClosed` — host-side, and never on the wire.
            // Unconditionally `Transport`, then and now; #944 changed only
            // which expiries are allowed to reach it.
            Self::Suspended(_) => FailureClass::Transport,
            // #930: a Scout refused a VM by a full pool used to be charged a
            // dispatch attempt, so three busy moments rejected a task nothing
            // had ever judged. The class comes off the *kind* vm-pool now
            // states on its refusal, never off the message, and
            // `FailureClass::for_service_error` is the one place the reading
            // is written down — the builder arm reads the same function.
            //
            // Only `Capacity` is waived, and the waiver is safe only because
            // something else stops the refused task being retried twice a
            // second: `dispatch` returns it to `Queued` (#967) and
            // `crate::pool_health` holds dispatch while the pool is full.
            // Waiving the strike removes the backstop that used to bound that
            // loop, so the hold is mandatory rather than preferable.
            Self::Client(ClientError::Service { kind, .. }) => {
                FailureClass::for_service_error(*kind)
            }
            Self::Store(_) | Self::Client(_) | Self::Timeout { .. } => FailureClass::Verdict,
        }
    }
}

/// What [`Scout::start`] hands back: a run that exists — a VM allocated, a
/// session row written — but has not been told what to do yet.
///
/// A struct rather than a tuple because the `Start` command is assembled from
/// both halves at one call site, and a bare `(VmId, String, String)` there is
/// two strings whose order nothing checks.
struct Started {
    vm_id: VmId,
    prompt: StartPrompt,
}

/// The two things the `Start` command carries that only the setup knows: the
/// rendered prompt, and the URL this run clones from (which is a lease, not the
/// repository's own address — see [`crate::broker`]).
struct StartPrompt {
    text: String,
    clone_url: String,
}

/// How this dispatcher boots a Scout VM. Uniform across dispatches — anything
/// that varies per task lives in [`ScoutTarget`].
#[derive(Debug, Clone)]
pub struct ScoutConfig {
    /// Image reference to allocate from vm-pool, e.g. `"agent:v1"`.
    pub image: String,
    /// VM configuration passed to vm-pool. The *shape* only — what a run
    /// authenticates with is minted per dispatch through `leases` and
    /// appended to this env at allocation, so no long-lived credential ever
    /// rides a VM's environment (#971).
    pub vm_config: VmConfig,
    /// Budget for one dispatch, measured from entry to [`Scout::dispatch`] so
    /// allocation is charged to it too. Measured on both the monotonic and the
    /// wall clock (see [`crate::deadline`]), so on expiry the VM is
    /// deallocated and the dispatch fails with [`ScoutError::Timeout`] — or,
    /// if the host was asleep for enough of it, [`ScoutError::Suspended`].
    pub timeout: Duration,
    /// Mints a run lease per dispatch when the target is
    /// [`CloneSource::Leased`]. `None` only where there is no broker at all —
    /// the integration tests, which dispatch against `file://` repos.
    pub leases: Option<LeaseIssuer>,
}

/// Least wall-clock budget a resumed run is given, however long the server was
/// down for.
///
/// The budget is otherwise measured from the run's original start, so a
/// restart cannot hand a hung scout a fresh hour. This floor covers the
/// opposite case: after a long outage the replay may already carry the
/// terminal event, and a budget of zero would deallocate the VM before that
/// outcome could be read.
const RESUME_MIN_BUDGET: Duration = Duration::from_secs(30);

/// The repository a single dispatch explores. Per-project, so one dispatcher
/// serves every tracked project.
#[derive(Debug, Clone)]
pub struct ScoutTarget {
    /// Where the scout-supervisor's `git clone` comes from: a lease minted at
    /// dispatch (production), or a literal URL (tests, `file://` repos).
    pub source: CloneSource,
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
    ///
    /// Two halves, and the split is where the failure handling changes hands.
    /// [`Scout::start`] owns everything from the `Scouting` transition through
    /// the session row; every failure inside it is a **pre-session** failure,
    /// with no row for `finalize_failed` to conclude — so this matches on it
    /// once and calls [`Scout::unwind_unstarted`], which is what puts the task
    /// back where the dispatcher can see it. Before that (#967) a refused
    /// allocation left the task in `Scouting` and `run::next_dispatchable`,
    /// which looks only at `Queued`, could not pick it up again until the next
    /// boot's reconciliation. Once there is a session row, the old paths own
    /// the failure exactly as they did.
    pub async fn dispatch(&self, task: Task, target: &ScoutTarget) -> Result<Spec, ScoutError> {
        info!(task_id = %task.id, "scout dispatch starting");
        // Anchored before anything else so the deadline covers allocation too,
        // making it a true run budget rather than a drain budget — and so a
        // host that suspends *during* allocation is caught, which the
        // `Instant` arithmetic this replaced could not see.
        let deadline = Deadline::starting_now(self.config.timeout);

        // Subscribe before allocating so no event for our VM can be missed.
        let mut events = self.client.subscribe_events();
        let session_id = SessionId::new();

        let Started { vm_id, prompt } = match self.start(&task, target, &session_id).await {
            Ok(started) => started,
            Err(e) => {
                self.unwind_unstarted(&task).await;
                return Err(e);
            }
        };

        // Send Start
        if let Err(e) = self
            .client
            .send_to_vm(
                &vm_id,
                TaskCommand::Scout(ScoutCommand::Start {
                    task_id: task.id.to_string(),
                    repo_clone_url: prompt.clone_url,
                    base_branch: target.base_branch.clone(),
                    prompt: prompt.text,
                }),
            )
            .await
        {
            self.revoke_lease(&session_id).await;
            self.finalize_failed(&session_id, &task, Some(&vm_id), format!("send: {e}"))
                .await?;
            return Err(e.into());
        }

        let app = AppEvents::live(&mut events, vm_id.clone());
        self.follow(&session_id, &task, &vm_id, app, &deadline, None)
            .await
    }

    /// Set a run up: claim the task, render its prompt, mint its credentials,
    /// allocate its VM and record its session.
    ///
    /// Everything here happens *before* there is a session row to conclude,
    /// which is the whole reason it is one function: its caller can then undo
    /// the one durable thing it did (the `Scouting` claim) in one place,
    /// whichever step failed.
    async fn start(
        &self,
        task: &Task,
        target: &ScoutTarget,
        session_id: &SessionId,
    ) -> Result<Started, ScoutError> {
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
        // Directions arrive on the `Task` this was handed, exactly as `prior`
        // and `salvage` are fetched above — so no signature changes and the
        // dispatch loop needs no edit. They are deliberately *not* cleared
        // here: see `Store::set_scout_directions`.
        if let Some(directions) = &task.scout_directions {
            info!(
                task_id = %task.id,
                author = directions.author.as_str(),
                bytes = directions.text.len(),
                "this run is directed"
            );
        }
        let prompt = render_prompt(
            task,
            prior.as_ref(),
            salvage.as_ref(),
            task.scout_directions.as_ref(),
            self.config.timeout,
        );

        // What this run authenticates with. A lease rather than the raw keys:
        // minted against this session, scoped to this repo, read-only, dead
        // minutes after the run — see `crate::broker`. Minting is a pre-agent
        // setup step, so a failure here is charged like any other (a store
        // that cannot insert a row fails identically every time).
        let (clone_url, credentials_env) = match (&target.source, &self.config.leases) {
            (CloneSource::Leased { repo }, Some(leases)) => {
                let grant = leases
                    .grant_agent(
                        SubjectKind::Scout,
                        session_id.as_str(),
                        repo,
                        self.config.timeout,
                    )
                    .await?;
                (grant.clone_url, grant.env)
            }
            // The broker cannot front a non-HTTP repo, but this run's API
            // credit still goes through it rather than riding raw.
            (CloneSource::Direct(url), Some(leases)) => (
                url.clone(),
                leases
                    .grant_anthropic_env(
                        SubjectKind::Scout,
                        session_id.as_str(),
                        self.config.timeout,
                    )
                    .await?,
            ),
            (CloneSource::Direct(url), None) => (url.clone(), Vec::new()),
            // A wiring bug, not a runtime condition: `run` always constructs
            // the issuer beside the leased targets. Surfaced as an ordinary
            // pre-session failure — the same shape as a store read failing
            // right above — so the dispatch loop's outcome recording handles
            // it like any other.
            (CloneSource::Leased { .. }, None) => {
                return Err(ScoutError::Store(StoreError::Invalid(
                    "leased dispatch with no lease issuer wired".into(),
                )));
            }
        };
        let mut vm_config = self.config.vm_config.clone();
        vm_config.env.extend(credentials_env);

        // Allocate
        let vm_id = self.client.allocate(&self.config.image, vm_config).await?;
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

        self.register(session_id, task, &vm_id, prompt, clone_url)
            .await
    }

    /// Record the session row for a VM that is already allocated.
    ///
    /// Hands the VM back if the row cannot be written: that window is the one
    /// place a leaked slot is invisible to all three reclamations — the pool
    /// holds a live VM, and no row anywhere names it.
    async fn register(
        &self,
        session_id: &SessionId,
        task: &Task,
        vm_id: &VmId,
        prompt: String,
        clone_url: String,
    ) -> Result<Started, ScoutError> {
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
            // A *copy*, not a reference: the task can be re-aimed tomorrow,
            // and this row has to keep saying what this run was told.
            directions: task.scout_directions.clone(),
        };
        if let Err(e) = self.store.insert_session(&session_row).await {
            self.revoke_lease(session_id).await;
            crate::teardown::deallocate_bounded(
                &self.client,
                &self.store,
                vm_id,
                "scout session row",
                crate::teardown::DEALLOCATE_TIMEOUT,
            )
            .await;
            return Err(e.into());
        }
        self.store
            .append_event(EventPayload::SessionStarted {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
            })
            .await?;

        Ok(Started {
            vm_id: vm_id.clone(),
            prompt: StartPrompt {
                text: prompt,
                clone_url,
            },
        })
    }

    /// Put a task back where the dispatcher can see it, after a failure that
    /// never got as far as a session row.
    ///
    /// Three details are deliberate. It **reads the state back** and only
    /// undoes an actual `Scouting`, because the failure may *be* the
    /// transition — this way it can only ever undo its own change. It is
    /// **best-effort**: this path is already failing, and a bookkeeping error
    /// here must not replace the error the caller is about to report. And it
    /// writes **no `Note`** — [`crate::run::record_outcome`] already appends
    /// one per failed dispatch, and a second would double every line on a feed
    /// the app and the orchestrator read.
    async fn unwind_unstarted(&self, task: &Task) {
        let state = match self.store.get_task(&task.id).await {
            Ok(Some(current)) => current.state,
            Ok(None) => return,
            Err(e) => {
                warn!(task_id = %task.id, error = %e, "could not read a task back to unwind it");
                return;
            }
        };
        if state != TaskState::Scouting {
            return;
        }
        if let Err(e) = self
            .store
            .update_task_state(&task.id, TaskState::Queued)
            .await
        {
            warn!(task_id = %task.id, error = %e, "could not return a task to the queue");
            return;
        }
        if let Err(e) = self
            .store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: TaskState::Scouting,
                to: TaskState::Queued,
            })
            .await
        {
            warn!(task_id = %task.id, error = %e, "could not log a task's return to the queue");
        }
        info!(task_id = %task.id, "a scout that never started returned its task to the queue");
    }

    /// Pick up a scout a previous process left running.
    ///
    /// **This always concludes `session`** — on success, on failure, and on
    /// "not resumable". [`Store::reconcile_orphaned_work_except`] skips rows
    /// handed to a reattach, so returning while leaving one `running` would
    /// strand it until someone noticed by hand.
    ///
    /// Everything that can go wrong degrades to the old behaviour rather than
    /// to a wedge: no VM on the row, an attach that fails, a VM the pool no
    /// longer has and no terminal event to show for it — each concludes the
    /// session exactly as reconciliation would have.
    pub async fn reattach(&self, session: Session, task: Task) -> Result<Spec, ScoutError> {
        let session_id = session.id.clone();
        let Some(vm_id) = session.vm_id.clone().map(VmId::new) else {
            let reason = "the session records no VM".to_string();
            self.finalize_failed(&session_id, &task, None, reason.clone())
                .await?;
            return Err(ScoutError::NotResumable(reason));
        };
        info!(session_id = %session_id, task_id = %task.id, %vm_id, "reattaching to a scout");

        // `attach` hands back the subscription it took *before* the snapshot;
        // taking one here would be the wrong one.
        let (mut events, resume) = match crate::reattach::attach(&self.client, &vm_id).await {
            Ok(attached) => attached,
            Err(e) => {
                let reason = format!("attach failed: {e}");
                self.finalize_failed(&session_id, &task, Some(&vm_id), reason.clone())
                    .await?;
                return Err(ScoutError::NotResumable(reason));
            }
        };

        // `present: false` is not "lost": the pool may have reaped the VM
        // after the run finished, in which case the terminal event is right
        // there in the replay and the whole run is still recoverable. Only
        // gone *and* silent is an orphan.
        if !resume.present && !resume.replay.iter().any(is_terminal) {
            let reason = "the VM is gone and its run never reported an outcome".to_string();
            self.finalize_failed(&session_id, &task, Some(&vm_id), reason.clone())
                .await?;
            return Err(ScoutError::NotResumable(reason));
        }

        // The budget is wall-clock from the original start, not from here —
        // a restart must not hand a hung scout a fresh hour. The floor exists
        // for the other case: after a long outage the replay may already hold
        // the terminal event, and a zero budget would deallocate a VM whose
        // outcome is in hand.
        let elapsed = (Utc::now() - session.started_at)
            .to_std()
            .unwrap_or_default();
        let remaining = self
            .config
            .timeout
            .saturating_sub(elapsed)
            .max(RESUME_MIN_BUDGET);
        let deadline = Deadline::starting_now(remaining);

        // The VM still holds the lease it was dispatched with — the only
        // token it will ever have — and a long outage may have expired it.
        // Re-arm it for the resumed budget; best-effort, because an
        // unextendable lease only means the agent's next API call 401s and
        // the run concludes the way it was already going to.
        if let Some(leases) = &self.config.leases {
            let until = Utc::now()
                + chrono::Duration::from_std(remaining + crate::broker::LEASE_SLACK)
                    .unwrap_or_default();
            if let Err(e) = leases
                .extend(SubjectKind::Scout, session_id.as_str(), until)
                .await
            {
                warn!(session_id = %session_id, error = %e, "could not extend the run's lease");
            }
        }

        // Rebuilt from the row and the notes table rather than from the
        // replay, because a bounded window is exactly what drops the oldest
        // events — `Started` first of all. This is why the branch is persisted
        // on arrival, and why checkpoints are persisted as they land.
        let state = DrainState {
            branch: (!session.branch.is_empty()).then(|| session.branch.clone()),
            checkpoint: self
                .store
                .get_scout_notes(&session_id)
                .await?
                .map(|notes| notes.notes),
            resumed: true,
            ..DrainState::default()
        };

        let app = AppEvents::resumed(&mut events, vm_id.clone(), resume);
        self.follow(&session_id, &task, &vm_id, app, &deadline, Some(state))
            .await
    }

    /// The second half of a run, shared by [`Scout::dispatch`] and
    /// [`Scout::reattach`]: drain to a terminal event, deallocate, finalize.
    ///
    /// `resumed` carries what a reattachment already knows from the session
    /// row (the branch, the last checkpoint) — `None` for a fresh dispatch,
    /// which starts from nothing.
    #[allow(clippy::too_many_arguments)]
    async fn follow(
        &self,
        session_id: &SessionId,
        task: &Task,
        vm_id: &VmId,
        mut events: AppEvents<'_>,
        deadline: &Deadline,
        resumed: Option<DrainState>,
    ) -> Result<Spec, ScoutError> {
        // Drain events until terminal Completed / Failed, or until the budget
        // runs out on either of its two clocks. An already-blown budget fires
        // immediately instead of wrapping.
        // #849 gave the transcript writer an owner; #856 added the checkpoint
        // writer beside it. Both survive: the sink is owner-addressed, and the
        // notes it salvages are still keyed to this session and task.
        let (mut sink, writer) =
            spawn_transcript_writer(self.store.clone(), TranscriptOwner::session(session_id));
        let (mut checkpoints, checkpoint_writer) =
            spawn_checkpoint_writer(self.store.clone(), session_id.clone(), task.id.clone());
        // Everything the drain loop learns lives out here, not inside the
        // future: `tokio::time::timeout` *drops* that future at the deadline,
        // and the deadline is the case this whole feature exists for. State
        // held inside it would be destroyed exactly when it is needed.
        let mut state = resumed.unwrap_or_default();
        if state.resumed {
            // One marker instead of the replayed transcript tail. There is no
            // durable watermark saying what the dead process already wrote, so
            // re-persisting the replay would silently double the tail; a
            // stated gap beats a doubled one.
            sink.push(
                TranscriptStream::Stderr,
                "[tasks] this run was picked back up after a server restart; \
                 output emitted while no server was listening is not repeated here"
                    .into(),
            );
        }
        // A cancel travels the same way the deadline does — see
        // `crate::cancel`: the drain is parked on a stream that a destroyed VM
        // would simply leave silent, so the interrupt has to reach *here*, and
        // teardown then goes down the path the deadline already uses.
        let result = match crate::cancel::bounded(
            &self.store,
            RunKind::Session,
            session_id.as_str(),
            deadline,
            drain_scout_events(
                &self.store,
                session_id,
                &self.config.image,
                &mut events,
                &mut sink,
                &mut checkpoints,
                &mut state,
            ),
        )
        .await
        {
            Bounded::Completed(result) => result,
            Bounded::Cancelled(request) => {
                warn!(
                    session_id = %session_id,
                    task_id = %task.id,
                    %vm_id,
                    actor = request.actor.as_str(),
                    "scout cancelled; tearing the VM down"
                );
                Err(ScoutError::Cancelled(request))
            }
            Bounded::TimedOut(expiry) => {
                self.note_expiry(task, vm_id, &expiry).await;
                if expiry.starved_by_suspend() {
                    Err(ScoutError::Suspended(expiry))
                } else {
                    // The *configured* budget, never `expiry.budget`: on the
                    // reattach path the effective budget is the remainder, and
                    // the integration tests pin specific numbers against this
                    // string.
                    Err(ScoutError::Timeout {
                        secs: self.config.timeout.as_secs(),
                    })
                }
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
            && let Err(e) = self.store.update_session_usage(session_id, usage).await
        {
            warn!(session_id = %session_id, error = %e, "recording session usage failed");
        }

        // Always try to deallocate, and never wait forever on it — the same
        // unbounded call that stalled the build queue holds a scout
        // concurrency slot here. Failures are not the dispatch's problem: the
        // pool frees the slot when that VM's event stream ends, and the next
        // daemon on its socket stops what is left. For a timeout this
        // *is* the cancel — there is deliberately no in-band Cancel command,
        // so freeing the slot means destroying the VM.
        crate::teardown::deallocate_bounded(
            &self.client,
            &self.store,
            vm_id,
            &format!("scout for task {}", task.id),
            crate::teardown::DEALLOCATE_TIMEOUT,
        )
        .await;

        // The run is over on every path through here, so its credit is too.
        // Tightening on top of expiry — the lease would die on its own.
        self.revoke_lease(session_id).await;

        let branch = state.branch.clone().unwrap_or_default();
        match result {
            Ok(DrainOutcome::Concluded {
                spec_markdown,
                files_touched,
            }) => {
                self.finalize_succeeded(
                    session_id,
                    task,
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
                class,
            }) => {
                let reason = format!("{reason}{}", unspent_budget_clause(deadline));
                self.finalize_stopped_early(
                    session_id,
                    task,
                    branch,
                    &reason,
                    notes_markdown,
                    files_touched,
                )
                .await?;
                Err(ScoutError::StoppedEarly { reason, class })
            }
            // A deliberate stop, and therefore neither a failure nor a run
            // that ended on its own terms: its own terminal path, so nothing
            // downstream has to infer intent from an error string.
            Err(ScoutError::Cancelled(request)) => {
                self.finalize_cancelled(
                    session_id,
                    task,
                    branch,
                    &request,
                    state.checkpoint.take(),
                )
                .await?;
                Err(ScoutError::Cancelled(request))
            }
            Err(e) => {
                // #982: the clause goes only on a supervisor verdict. A
                // `Timeout` has no unspent budget by construction and its
                // string is pinned by the integration tests, and a `Suspended`
                // run's unspent budget is already what its own sentence is
                // about.
                let reason = match &e {
                    ScoutError::ScoutFailed { .. } => {
                        format!("{e}{}", unspent_budget_clause(deadline))
                    }
                    _ => format!("{e}"),
                };
                // The deadline and a dead stream never reach the supervisor's
                // own reporting — the VM is destroyed where it stands. The
                // last checkpoint is all there is, and it is worth the same
                // as one the supervisor handed over deliberately.
                //
                // The error is returned unchanged either way. `Timeout` in
                // particular keeps its shape: [`crate::deadline`] and the timeout
                // integration tests pin `exit_reason` containing "timed out",
                // and a salvaged timeout is still a timeout. A `Suspended` run
                // travels the same path and keeps the same salvage — the notes
                // a scout streamed before the lid closed are the one thing the
                // suspend did not cost it.
                match state.checkpoint.take() {
                    Some(notes) => {
                        self.finalize_stopped_early(
                            session_id,
                            task,
                            branch,
                            &reason,
                            notes,
                            Vec::new(),
                        )
                        .await?
                    }
                    None => {
                        self.finalize_failed(session_id, task, Some(vm_id), reason)
                            .await?
                    }
                }
                Err(e)
            }
        }
    }

    /// Revoke this session's lease, when leases are wired at all.
    /// Best-effort tightening on top of expiry — see
    /// [`LeaseIssuer::revoke_best_effort`].
    async fn revoke_lease(&self, session_id: &SessionId) {
        if let Some(leases) = &self.config.leases {
            leases
                .revoke_best_effort(SubjectKind::Scout, session_id.as_str())
                .await;
        }
    }

    /// Breadcrumb naming the deadline, written at expiry so the vm id and the
    /// budget land in the entry. Best-effort on purpose: a failed breadcrumb
    /// must not skip the deallocation that is the whole point of the deadline.
    ///
    /// One function for both sentences — a spent budget and a host that slept
    /// through enough of one — so the event-log note and the `exit_reason` the
    /// caller writes cannot come to different conclusions about the same event.
    ///
    /// The timeout branch has a third case inside it since #944: a run killed
    /// at a wake with budget still on the monotonic clock, which is charged but
    /// is not a budget the scout ran all of. It gets the expiry's own clause
    /// appended, and only then — a run that genuinely spent its budget awake
    /// says nothing about clocks at all.
    async fn note_expiry(&self, task: &Task, vm_id: &VmId, expiry: &Expiry) {
        let message = if expiry.starved_by_suspend() {
            warn!(
                task_id = %task.id,
                %vm_id,
                slept = %crate::deadline::human(expiry.suspended()),
                awake = %crate::deadline::human(expiry.awake),
                "the host was suspended while a scout was running"
            );
            format!(
                "scout for {} abandoned: {expiry}; deallocating {vm_id}",
                task.id
            )
        } else {
            let secs = self.config.timeout.as_secs();
            warn!(
                task_id = %task.id,
                %vm_id,
                timeout_secs = secs,
                slept = %crate::deadline::human(expiry.suspended()),
                awake = %crate::deadline::human(expiry.awake),
                "scout timed out"
            );
            // Only when something went unspent: appending the clause to a run
            // that had its whole budget awake would have it report on clocks
            // that had nothing to say.
            let clause = if expiry.unspent().is_zero() {
                String::new()
            } else {
                format!(" ({expiry})")
            };
            format!(
                "scout for {} timed out after {secs}s{clause}; deallocating {vm_id}",
                task.id
            )
        };
        if let Err(e) = self
            .store
            .append_event(EventPayload::Note {
                source: crate::run::DISPATCHER.into(),
                message,
            })
            .await
        {
            warn!(task_id = %task.id, error = %e, "could not record the expiry note");
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
            session_id: Some(session_id.clone()),
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
                session_id: Some(session_id.clone()),
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

    /// A run somebody stopped: the third terminal path, and the only one whose
    /// reason names a person.
    ///
    /// Three things make it different from a failure, and each is deliberate:
    ///
    /// - **The status is `cancelled`, not `scout_stopped_early`.** That status
    ///   means the run ended on its own terms and charges the task an attempt,
    ///   and neither is true here.
    /// - **The salvage is kept.** The checkpoint writer has already persisted
    ///   whatever the scout had written down; the cancel only stamps its own
    ///   rationale onto the notes' `reason`, so the next attempt reads both
    ///   the leads and why the last look was called off.
    /// - **The task goes back to `backlog`, not `queued`.** Every other
    ///   non-success path returns it to `queued` on the principle that
    ///   picked-up work stays picked up. A cancel is the one case where that is
    ///   wrong: the dispatch loop would start a fresh scout for it inside half
    ///   a second, which is a restart nobody asked for. Re-queueing is one call
    ///   by the person who stopped it — see [`Store::return_task_to_backlog`].
    async fn finalize_cancelled(
        &self,
        session_id: &SessionId,
        task: &Task,
        branch: String,
        request: &CancelRequest,
        checkpoint: Option<String>,
    ) -> Result<(), ScoutError> {
        let now = Utc::now();
        let reason = request.exit_reason();
        if !branch.is_empty() {
            self.store
                .update_session_branch(session_id, &branch)
                .await?;
        }

        // Two shapes of the same salvage. A checkpoint still in hand is written
        // whole (it may be newer than the persisted row); otherwise the reason
        // is stamped onto whatever the checkpoint writer already landed, which
        // is a no-op when there is nothing to stamp.
        match checkpoint {
            Some(notes) => {
                self.store
                    .upsert_scout_notes(&ScoutNotes {
                        session_id: session_id.clone(),
                        task_id: task.id.clone(),
                        reason: Some(reason.clone()),
                        notes,
                        files_touched: Vec::new(),
                        updated_at: now,
                    })
                    .await?
            }
            None => {
                self.store
                    .stamp_scout_notes_reason(session_id, &reason)
                    .await?
            }
        }

        self.store
            .update_session_completion(
                session_id,
                SessionStatus::Cancelled,
                now,
                Some(reason.clone()),
            )
            .await?;
        // Emits its own TaskStateChanged, whatever state the task was in.
        self.store.return_task_to_backlog(&task.id).await?;
        self.store
            .append_event(EventPayload::SessionCompleted {
                session_id: session_id.clone(),
                task_id: task.id.clone(),
                status: SessionStatus::Cancelled,
            })
            .await?;
        warn!(task_id = %task.id, session_id = %session_id, reason, "scout cancelled");
        Ok(())
    }

    /// `vm_id` is `None` when there is no VM to name — a session row that
    /// never recorded one. It is a log field only.
    async fn finalize_failed(
        &self,
        session_id: &SessionId,
        task: &Task,
        vm_id: Option<&VmId>,
        reason: String,
    ) -> Result<(), ScoutError> {
        // Failure reasons quote git, and git quotes the credentialed clone URL
        // back at us. One call here covers all three destinations below:
        // `sessions.exit_reason`, the event log, and the log line.
        let reason = crate::redact::redact_owned(reason);
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
        warn!(task_id = %task.id, ?vm_id, reason, "scout failed");
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
        /// The supervisor's own answer to whether the run judged the work.
        /// Carried through the drain rather than re-derived, because this is
        /// the only place it is known.
        class: FailureClass,
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
    /// Whether this state was rebuilt for a run picked back up after a
    /// restart, rather than accumulated from the start of one.
    resumed: bool,
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
            // Retried because this task is detached and has no caller to
            // return an error to — and because what it is persisting is the
            // salvage a cut-short run is judged on, which is the one artefact
            // `NOTES.md` streaming exists to protect. The retry is a belt;
            // `store::begin_write` is what removed the unretryable failure.
            let write = crate::store::retry_on_contention(|| store.upsert_scout_notes(&row)).await;
            if let Err(e) = write {
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

/// Whether a scout event ends the run. Used to decide whether a VM the pool
/// no longer has still left a recoverable outcome behind.
fn is_terminal(event: &TaskEvent) -> bool {
    matches!(
        event,
        TaskEvent::Scout(
            ScoutEvent::Completed { .. }
                | ScoutEvent::StoppedEarly { .. }
                | ScoutEvent::Failed { .. }
        )
    )
}

/// Consume this run's events — the replay first if there is one, then live —
/// until its VM reports a terminal Completed/Failed. Events from other VMs
/// (concurrent scouts) are filtered out by [`AppEvents`]; service errors for
/// our requests surface as [`ClientError`] on the calls themselves, not here.
///
/// A replayed event may already have been acted on by the process that died,
/// so it is used to rebuild state and never to append output — see
/// [`Origin`].
async fn drain_scout_events(
    store: &Store,
    session_id: &SessionId,
    // The image reference this run was allocated from. The `Started` event
    // says what is *inside* it; only the host knows what it asked for.
    image: &str,
    events: &mut AppEvents<'_>,
    sink: &mut TranscriptSink,
    checkpoints: &mut CheckpointSink,
    state: &mut DrainState,
) -> Result<DrainOutcome, ScoutError> {
    loop {
        let (origin, event) = events.next().await.ok_or(ScoutError::StreamClosed)?;

        match event {
            TaskEvent::Scout(app) => match app {
                ScoutEvent::Started {
                    branch: b,
                    supervisor,
                } => {
                    // What the image is running, from the only moment there is
                    // to ask it: the VM exists only while this run is inside
                    // it. `None` is the loudest answer, not the quietest — see
                    // `ImageFreshness::Unstamped`.
                    crate::images::observe(
                        store,
                        image,
                        tasks_api::version::ImageRole::Scout,
                        supervisor.as_ref(),
                        session_id.as_str(),
                    )
                    .await;
                    state.branch = Some(b.clone());
                    // Persisted here rather than at finalize: a bounded replay
                    // window drops the *oldest* events, and `Started` is the
                    // first one a run emits. A reattachment therefore has to
                    // be able to read the branch off the row, which means the
                    // row has to have it from the moment it is known.
                    if let Err(e) = store.update_session_branch(session_id, &b).await {
                        warn!(session_id = %session_id, error = %e, "persisting the scout branch failed");
                    }
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
                    // Replayed output is the tail of a transcript the previous
                    // process already wrote, with no watermark saying how much
                    // of it landed. Persisting it again would duplicate that
                    // tail silently; `follow` states the gap once instead.
                    if origin == Origin::Live {
                        sink.push(transcript_stream(stream), line);
                    }
                }
                // Kept twice over: persisted (so it survives this process
                // dying) and held in `state` (so it survives this future being
                // dropped at the deadline). Neither covers the other. A
                // replayed checkpoint is worth re-persisting — the write is an
                // upsert of the newest notes, not an append.
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
                    class,
                } => {
                    return Ok(DrainOutcome::StoppedEarly {
                        reason,
                        notes_markdown,
                        files_touched,
                        class,
                    });
                }
                ScoutEvent::Failed { reason, class } => {
                    return Err(ScoutError::ScoutFailed { reason, class });
                }
            },
            // A Builder VM's traffic on the same connection. Not ours.
            TaskEvent::Build(_) => {}
        }
    }
}

/// A clause naming a budget the run did not spend, when most of it is left.
///
/// #982: a Scout that wrote the fix, confirmed it and then ended its turn at
/// 18 minutes of a 60-minute budget is recorded as `SPEC.md not found at …` —
/// the same sentence a run killed at the deadline gets. They are different
/// events. One is an agent that explored and could not conclude, which is a
/// verdict; the other is an agent that believed it would be resumed, which is
/// a fact about this harness. A human reading `sessions.exit_reason` cannot
/// tell them apart, and this is the one place that can say so cheaply.
///
/// It changes **no decision**: `FailureClass` is stamped off a field and never
/// off reason text, which is the rule this codebase follows everywhere. This
/// is addressed to a human.
///
/// Half, and deliberately not `WAIVED_BUDGET_SHARE` (a quarter): that constant
/// answers whether a run was *given* its budget, this one answers whether it
/// chose to stop, and one number serving two questions is #944 exactly.
fn unspent_budget_clause(deadline: &Deadline) -> String {
    let Some(remaining) = deadline.remaining() else {
        return String::new();
    };
    if remaining * 2 < deadline.budget() {
        return String::new();
    }
    format!(
        " — the run ended on its own terms with {} of its {} budget unspent, so this was \
         not the deadline",
        crate::deadline::human(remaining),
        crate::deadline::human(deadline.budget()),
    )
}
/// Build the scout prompt, splicing in the previous attempt when the task has
/// one. The section sits between the issue body and the instructions so the
/// model reads issue → what went wrong last time → what to do.
fn render_prompt(
    task: &Task,
    prior: Option<&ReviewedSpec>,
    salvage: Option<&ScoutNotes>,
    directions: Option<&Directions>,
    budget: Duration,
) -> String {
    let previous = prior.map(render_previous_attempt).unwrap_or_default();
    let field_notes = salvage.map(render_field_notes).unwrap_or_default();
    // Last before the instructions, so the model reads issue → what went wrong
    // last time → unverified leads → what it has additionally been told → what
    // to do. Nothing is emitted at all when there are no directions: an
    // always-present empty heading is exactly what teaches an agent to skim
    // past the one that matters.
    let directions = directions.map(render_directions).unwrap_or_default();
    format!(
        "You are a Scout in the Double Diamond architecture.\n\n\
         ## Issue: {title} (#{num})\n\n\
         {body}\n\n\
         {previous}\
         {field_notes}\
         {directions}\
         ## Your job\n\n\
         1. Implement a working solution in the cloned repo (cwd).\n\
         2. Keep `NOTES.md` in the repo root up to date as you go: findings, \
         dead ends, where things live, anything you would hate to re-derive. \
         It is read back every 30 seconds and is the only thing that survives \
         if this run is cut short, so write it as you learn rather than at the \
         end.\n\
         3. Verify your conclusion, in proportion to what you changed — the \
         tests that cover it, not the whole suite by reflex. A cold build here \
         can eat most of your budget, and it buys nothing for a change the \
         suite does not exercise.\n\
         4. Write `SPEC.md` in the repo root with the structure below, and \
         only once you have actually concluded. **`SPEC.md` is not a \
         checkpoint.** A half-written spec is worse than no spec, because it \
         reaches a reviewer looking finished. If you want to record progress, \
         that is what `NOTES.md` is for.\n\
         5. Do NOT create a PR or push anywhere.\n\n\
         {pipe_clause}\n\n\
         ## Two things about this run that are not true of an ordinary session\n\n\
         **You have {budget_mins} minutes, once.** That is the whole run — \
         the clone before you started, this turn, and the packaging after it \
         — measured on the wall clock from dispatch. There is no later: when \
         you end your turn the run is over, and when the budget runs out the \
         machine is destroyed where it stands. Nothing you have not already \
         sent out survives that. Two things follow, and they are the two \
         mistakes available to an agent that cannot see its own clock. A \
         backgrounded command buys you nothing — its child is killed with the \
         turn — so anything whose result you need must be awaited inline, \
         however long it takes; if you find yourself writing a poll loop over \
         a file another process will write, stop, because it can only report \
         to a turn that has already ended. And do not start something that \
         cannot finish: a cold build in a large workspace can run forty \
         minutes, so before you launch one, ask what it will cost against \
         what is left, and spend the remainder writing down what you know \
         instead if the answer is that it will not fit.\n\n\
         **Draft the spec early, in `NOTES.md`.** Once you have a shape in \
         mind and before you finish implementing, write a complete first draft \
         of the spec — the whole structure below, filled in as best you can — \
         into `NOTES.md`, and revise it there as you learn. It costs a few \
         minutes and it is the difference between a run that is cut short \
         having produced nothing and one whose successor starts from your \
         design. `NOTES.md` is streamed out while you work; `SPEC.md` is not \
         read until you finish, so a spec written only at the end is a spec \
         that a timeout takes with it. Keep it in `NOTES.md` until you have \
         actually concluded, then write it to `SPEC.md` — the draft is your \
         working copy, and the file name is what tells a reviewer which one \
         they are looking at.\n\n\
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
        directions = directions,
        budget_mins = budget.as_secs() / 60,
        // Deliberately above the `## Two things` heading rather than under
        // it: that heading counts its own contents, and a pipe reporting the
        // wrong status is true of every shell everywhere rather than of this
        // run. It sits with step 3 — verifying the conclusion — which is
        // where a Scout decides whether its change works.
        pipe_clause = crate::prompt::PIPE_EXIT_STATUS,
    )
}

/// Render the `## Directions for this exploration` section.
///
/// Framed as the opposite of the field notes above it: those are explicitly
/// unverified leads, these are an instruction from a named author that the
/// run is expected to follow. Getting the two voices the same way round is
/// most of the value of having two sections.
fn render_directions(directions: &Directions) -> String {
    let text = trim_prompt_text("directions", directions.text.trim());
    format!(
        "## Directions for this exploration\n\n\
         {author} added the following when sending this task to a Scout. It is \
         **not** part of the issue, and no reviewer has seen it — it is \
         addressed to you.\n\n\
         Treat it as a requirement, not a suggestion. The issue above is still \
         what is being solved; these directions say how to go about it. If one \
         of them genuinely conflicts with the issue, resolve it in the \
         directions' favour **and say so in `SPEC.md`**, because the reviewer \
         reads the issue and cannot see this section.\n\n\
         Account for every direction in `SPEC.md`'s `### Notes` — including \
         any you decided against, and why. A direction you silently dropped is \
         indistinguishable from one you never read.\n\n\
         {text}\n\n",
        author = directions.author_phrase(),
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
    let notes = trim_prompt_text("field notes", salvage.notes.trim());
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

/// Cut quoted prompt text to [`MAX_PROMPT_NOTES_BYTES`] on a char boundary,
/// keeping the head and saying so.
///
/// `what` names the section in the marker, because more than one thing is
/// quoted into this prompt now and a truncation notice that says "field notes"
/// under a `## Directions` heading is worse than no notice: it sends the
/// reader looking for a section that was never cut.
///
/// Directions use this as a backstop only — the API refuses an oversized set
/// with a 400 rather than silently shortening one, so anything arriving here
/// predates that check or came in some other way.
fn trim_prompt_text(what: &str, notes: &str) -> String {
    if notes.len() <= MAX_PROMPT_NOTES_BYTES {
        return notes.to_string();
    }
    let mut cut = MAX_PROMPT_NOTES_BYTES;
    while cut > 0 && !notes.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = notes.len() - cut;
    format!(
        "{}\n\n…[tasks: {what} truncated here, {dropped} bytes dropped]",
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
         Account for every item of it in `SPEC.md`'s `### Notes` — including any \
         you decided against, and why. An item you silently dropped is \
         indistinguishable from one you never read.\n\n\
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
    use crate::models::Actor;

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
            scout_directions: None,
        }
    }

    fn reviewed(content: &str, feedback: Option<&str>) -> ReviewedSpec {
        ReviewedSpec {
            spec: Spec {
                id: SpecId::new(),
                session_id: Some(SessionId::new()),
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

    /// The scout half of the `StreamClosed` move. A scout is already spared
    /// its attempt by a *different* guard — `crate::run::is_disconnect`
    /// returns before `failure_class` is consulted — so this pins the
    /// classification rather than a behaviour change, which is exactly the
    /// point: two answers about the same error must not disagree.
    ///
    /// The negative half is kept for the reason it is kept next door: an
    /// assertion that nothing was charged reads identically to the cap being
    /// switched off unless something in the same test still gets charged.
    #[test]
    fn a_closed_event_stream_is_transport_and_costs_the_task_no_attempt() {
        use crate::store::Strike;

        assert_eq!(
            ScoutError::StreamClosed.failure_class(),
            FailureClass::Transport,
        );
        assert!(!ScoutError::StreamClosed.failure_class().is_verdict());
        assert_eq!(
            Strike::for_class(ScoutError::StreamClosed.failure_class()),
            Strike::Waive,
        );

        // Still charged: a run that concluded with nothing usable, and one
        // that had the entire budget awake.
        assert_eq!(
            Strike::for_class(
                ScoutError::ScoutFailed {
                    reason: "SPEC.md not found".into(),
                    class: FailureClass::Verdict,
                }
                .failure_class()
            ),
            Strike::Charge,
        );
        assert_eq!(
            Strike::for_class(ScoutError::Timeout { secs: 1 }.failure_class()),
            Strike::Charge,
        );
    }

    /// #930: a Scout refused a VM by a full pool is not a Scout that judged
    /// the work. The class comes off the `kind` vm-pool states on its refusal,
    /// so `pool exhausted` and `no such image` — which differ only as prose —
    /// get different answers.
    ///
    /// The negative half is the whole rest of the vocabulary, `Unspecified`
    /// above all: that is what a vm-pool older than the field says, and it is
    /// the routine case, so a waiver there would silently spare every
    /// permanent misconfiguration on every old daemon.
    #[test]
    fn a_full_pool_is_transport_and_every_other_refusal_still_charges() {
        use crate::store::Strike;
        use vm_pool_protocol::ServiceErrorKind;

        fn refused(kind: ServiceErrorKind) -> ScoutError {
            ScoutError::Client(ClientError::Service {
                message: "allocate failed: pool exhausted: 0 available, 1 requested".into(),
                kind,
            })
        }

        assert_eq!(
            refused(ServiceErrorKind::Capacity).failure_class(),
            FailureClass::Transport
        );
        assert_eq!(
            Strike::for_class(refused(ServiceErrorKind::Capacity).failure_class()),
            Strike::Waive
        );

        for kind in [
            ServiceErrorKind::Unspecified,
            ServiceErrorKind::Image,
            ServiceErrorKind::Runtime,
            ServiceErrorKind::NoSuchVm,
            ServiceErrorKind::NotReady,
            ServiceErrorKind::Transport,
            ServiceErrorKind::BadRequest,
            ServiceErrorKind::Other,
        ] {
            assert_eq!(
                Strike::for_class(refused(kind).failure_class()),
                Strike::Charge,
                "{kind} must still cost the task an attempt"
            );
        }

        // And an ordinary empty-handed run is untouched by any of this.
        assert_eq!(
            Strike::for_class(
                ScoutError::ScoutFailed {
                    reason: "SPEC.md not found".into(),
                    class: FailureClass::Verdict,
                }
                .failure_class()
            ),
            Strike::Charge,
        );
    }

    /// #929, on the scout side. A budget the host slept through is not a budget
    /// the run spent, and the two clocks are what can tell them apart — so it
    /// is `Transport` and costs the task nothing.
    ///
    /// The negative half is the same one next door: a deadline genuinely spent
    /// awake still charges, or "nothing was charged" would read as the cap
    /// having been switched off.
    #[tokio::test]
    async fn a_suspended_host_is_transport_and_costs_the_task_no_attempt() {
        use crate::store::Strike;

        let expiry =
            Deadline::suspended_for(Duration::from_secs(3600), Duration::from_secs(8 * 3600))
                .expired()
                .await;
        let suspended = ScoutError::Suspended(expiry);

        assert_eq!(suspended.failure_class(), FailureClass::Transport);
        assert_eq!(Strike::for_class(suspended.failure_class()), Strike::Waive,);
        // And it must not be mistakable for the thing it replaces: two
        // integration tests match `exit_reason` on this substring.
        let reason = suspended.to_string();
        assert!(!reason.contains("timed out"), "{reason}");
        assert!(reason.contains("the host was suspended"), "{reason}");

        // The negative half: a budget spent awake is still a verdict.
        assert_eq!(
            Strike::for_class(ScoutError::Timeout { secs: 3600 }.failure_class()),
            Strike::Charge,
        );
        assert_eq!(
            ScoutError::Timeout { secs: 3600 }.to_string(),
            "scout timed out after 3600s"
        );
    }

    /// #944: a host that napped is not a host that starved the run. A scout
    /// that slept for 61 seconds somewhere inside an hour and spent the rest of
    /// it working routes to `Timeout` and is charged, exactly as it would be if
    /// the lid had never moved.
    #[test]
    fn a_scout_that_merely_napped_is_still_charged() {
        use crate::store::Strike;

        let napped = Expiry {
            budget: Duration::from_secs(3600),
            elapsed: Duration::from_secs(3600 + 61),
            awake: Duration::from_secs(3600),
        };
        assert!(!napped.starved_by_suspend(), "{napped:?}");
        assert_eq!(
            Strike::for_class(ScoutError::Timeout { secs: 3600 }.failure_class()),
            Strike::Charge,
        );
    }

    /// #1046: a Scout finished the work, verified it, and was killed six
    /// minutes later with `SPEC.md` never created — because `SPEC.md` is
    /// written last and is not read until the run ends. `NOTES.md` is streamed
    /// out while the run is alive, so the draft goes there.
    ///
    /// The draft goes to `NOTES.md` and **not** to an early `SPEC.md`, which
    /// is the fix that looks equivalent and is not: `SPEC.md` means "I
    /// concluded", and a draft sitting under that name when an agent ends its
    /// turn early would reach the review queue looking finished — the exact
    /// thing the rule beside it exists to prevent. Salvage is never a spec,
    /// and promoting one stays a human act.
    #[test]
    fn the_prompt_asks_for_the_spec_to_be_drafted_where_it_will_survive() {
        let prompt = render_prompt(&task_fixture(), None, None, None, Duration::from_secs(3600));
        assert!(
            prompt.contains("Draft the spec early, in `NOTES.md`"),
            "{prompt}"
        );
        assert!(
            prompt.contains("`SPEC.md` is not a checkpoint"),
            "and the rule that makes the draft go to NOTES.md is still there: {prompt}"
        );
    }

    /// #1071: the third clause of that kind and the worst of them, because
    /// the other two lose a run while this one produces a false pass. Pinned
    /// as the whole const rather than a keyword, so a paraphrase or a dropped
    /// splice goes red — and pinned on the near side of the `## Two things`
    /// heading, because that heading counts its own contents and a pipe
    /// reporting the wrong status is true of every shell everywhere.
    #[test]
    fn the_prompt_says_a_pipe_reports_the_pipes_exit_status() {
        let prompt = render_prompt(&task_fixture(), None, None, None, Duration::from_secs(3600));
        assert!(prompt.contains(crate::prompt::PIPE_EXIT_STATUS), "{prompt}");
        let (before, after) = prompt
            .split_once("## Two things about this run")
            .expect("the heading is still there");
        assert!(
            before.contains(crate::prompt::PIPE_EXIT_STATUS),
            "the clause belongs with step 3, above the heading that counts \
             its own contents: {prompt}"
        );
        assert!(!after.contains(crate::prompt::PIPE_EXIT_STATUS), "{prompt}");
    }

    /// #962: an agent backgrounded three `until … sleep 20` waiters over a
    /// cold build and returned its turn, saying it would pick the result up
    /// later. There is no later in a `--print` run: the turn ending is the run
    /// ending, and the children die with it. The orchestrator's own prompt has
    /// carried this sentence for a while; the agents that most need it were
    /// never told.
    #[test]
    fn the_prompt_says_a_backgrounded_command_dies_with_the_turn() {
        let prompt = render_prompt(&task_fixture(), None, None, None, Duration::from_secs(3600));
        assert!(
            prompt.contains("backgrounded command buys you nothing"),
            "{prompt}"
        );
        assert!(prompt.contains("awaited inline"), "{prompt}");
        // And the incentive that produced the backgrounding in the first
        // place: a whole-suite build for a change the suite does not exercise.
        assert!(
            prompt.contains("in proportion to what you changed"),
            "{prompt}"
        );
    }

    /// #982 (1): an agent that cannot see its clock makes the two scheduling
    /// mistakes available to it — waiting for a result that will never be
    /// collected, and starting something that cannot finish. The budget is a
    /// number the host has and the agent did not.
    #[test]
    fn the_prompt_tells_the_agent_how_long_it_has() {
        let prompt = render_prompt(&task_fixture(), None, None, None, Duration::from_secs(3600));
        assert!(prompt.contains("You have 60 minutes, once"), "{prompt}");
        assert!(prompt.contains("do not start something that"), "{prompt}");
    }

    /// The clause is rendered from the budget it was given, not from a
    /// constant — a reattached run's budget is the remainder, and a prompt
    /// naming the configured hour would be lying to it.
    #[test]
    fn the_clock_the_prompt_names_is_the_one_the_run_was_given() {
        let prompt = render_prompt(
            &task_fixture(),
            None,
            None,
            None,
            Duration::from_secs(15 * 60),
        );
        assert!(prompt.contains("You have 15 minutes, once"), "{prompt}");
    }

    /// #982 (4): `SPEC.md not found` at 18 minutes of a 60-minute budget and
    /// the same sentence at the deadline are different events and read
    /// identically. Nothing decides on this text — `FailureClass` is stamped
    /// off a field — so it costs nothing and it stops a human misreading the
    /// ledger.
    #[tokio::test]
    async fn a_run_that_stopped_with_most_of_its_budget_left_says_so() {
        let deadline = Deadline::starting_now(Duration::from_secs(3600));
        let clause = unspent_budget_clause(&deadline);
        assert!(clause.contains("was not the deadline"), "{clause}");
        assert!(clause.contains("unspent"), "{clause}");

        // And silent once most of the budget is gone: a run that used its
        // hour and produced nothing is a verdict, and saying `not the
        // deadline` there would be the same misreading in the other
        // direction.
        let nearly_done = Deadline::starting_now(Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(unspent_budget_clause(&nearly_done), "");
    }
    #[test]
    fn a_fresh_prompt_has_no_previous_attempt_section() {
        let prompt = render_prompt(&task_fixture(), None, None, None, Duration::from_secs(3600));
        assert!(!prompt.contains("Previous attempt"));
        // The body must still run straight into the instructions.
        assert!(prompt.contains("The issue body.\n\n## Your job"));
        // No empty heading either: a `## Directions` that is always there is
        // exactly what teaches an agent to skim past the one that matters.
        assert!(!prompt.contains("## Directions"), "{prompt}");
    }

    #[test]
    fn directions_sit_last_before_the_job_and_name_their_author() {
        let directions = Directions::new("start from the poller, not the API", Actor::Human);
        let notes = salvaged("half an idea", Some("the VM went away"));
        let prompt = render_prompt(
            &task_fixture(),
            None,
            Some(&notes),
            Some(&directions),
            Duration::from_secs(3600),
        );

        let field = prompt.find("## Field notes from an interrupted").unwrap();
        let section = prompt.find("## Directions for this exploration").unwrap();
        let job = prompt.find("## Your job").unwrap();
        assert!(field < section && section < job, "{prompt}");

        assert!(
            prompt.contains("The human running this pipeline"),
            "{prompt}"
        );
        assert!(
            prompt.contains("start from the poller, not the API"),
            "{prompt}"
        );
        assert!(
            prompt.contains("a requirement, not a suggestion"),
            "{prompt}"
        );
        // Accounted for where a Scout writes, declines included.
        assert!(prompt.contains("### Notes"), "{prompt}");
        assert!(prompt.contains("decided against"), "{prompt}");

        let orchestrated = render_prompt(
            &task_fixture(),
            None,
            None,
            Some(&Directions::new("x", Actor::Orchestrator)),
            Duration::from_secs(3600),
        );
        assert!(
            orchestrated.contains("The orchestrator agent"),
            "{orchestrated}"
        );
    }

    /// Directions and field notes are the two quoted sections, and they are
    /// framed as opposites on purpose: notes are unverified leads, directions
    /// are an instruction to follow. A directions-only prompt must carry none
    /// of the salvage's hedging voice.
    #[test]
    fn directions_are_not_framed_as_unverified_salvage() {
        let prompt = render_prompt(
            &task_fixture(),
            None,
            None,
            Some(&Directions::new("do the thing", Actor::Human)),
            Duration::from_secs(3600),
        );
        assert!(
            !prompt.contains("Nothing below has been verified"),
            "{prompt}"
        );
        assert!(!prompt.contains("Field notes"), "{prompt}");
        assert!(
            prompt.contains("no reviewer has seen it"),
            "it is still not a reviewed artifact, and says so in its own words: {prompt}"
        );
    }

    #[test]
    fn a_re_scout_prompt_carries_the_verdict_feedback_and_prior_spec() {
        let prior = reviewed(
            "## Spec: old\n\nSection 3 is thin.",
            Some("Flesh out section 3."),
        );
        let prompt = render_prompt(
            &task_fixture(),
            Some(&prior),
            None,
            None,
            Duration::from_secs(3600),
        );

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

        // The section already called the feedback a requirement; it had
        // nowhere for a *refusal* to land, which is the symmetric silent drop
        // #935 closed on the Builder side.
        assert!(
            prompt.contains("a requirement, not a suggestion"),
            "{prompt}"
        );
        assert!(prompt.contains("`SPEC.md`'s `### Notes`"), "{prompt}");
        assert!(prompt.contains("decided against"), "{prompt}");
    }

    #[test]
    fn missing_or_blank_feedback_still_renders() {
        for empty in [None, Some(""), Some("   ")] {
            let prompt = render_prompt(
                &task_fixture(),
                Some(&reviewed("spec body", empty)),
                None,
                None,
                Duration::from_secs(3600),
            );
            assert!(prompt.contains("## Previous attempt"));
            assert!(prompt.contains("no written feedback"));
        }
    }

    #[test]
    fn the_fence_outlives_fences_nested_in_the_quoted_spec() {
        // A spec containing its own ```rust block would break out of a plain
        // ``` wrapper and merge its headings into the prompt's structure.
        let nested = "## Spec\n\n```rust\nfn x() {}\n```\n";
        let prompt = render_prompt(
            &task_fixture(),
            Some(&reviewed(nested, Some("f"))),
            None,
            None,
            Duration::from_secs(3600),
        );
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
        let prompt = render_prompt(&task_fixture(), None, None, None, Duration::from_secs(3600));
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
        let prompt = render_prompt(
            &task_fixture(),
            None,
            Some(&notes),
            None,
            Duration::from_secs(3600),
        );

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
        let no_reason = render_prompt(
            &task_fixture(),
            None,
            Some(&salvaged("x", None)),
            None,
            Duration::from_secs(3600),
        );
        assert!(no_reason.contains("cut short before it could say why"));
        assert!(!no_reason.contains("None)"));
    }

    /// Notes containing their own fenced code must not break out of the
    /// wrapper and merge into the prompt's structure — same trap as the
    /// quoted prior spec.
    #[test]
    fn quoted_notes_survive_their_own_fences() {
        let notes = salvaged("```rust\nfn x() {}\n```", None);
        let prompt = render_prompt(
            &task_fixture(),
            None,
            Some(&notes),
            None,
            Duration::from_secs(3600),
        );
        assert!(prompt.contains("````markdown"));
    }

    /// The transport cap is 256 KiB; a prompt cap of the same size would
    /// spend a retry's context window on the thing it was meant to help.
    #[test]
    fn prompt_notes_are_trimmed_head_first() {
        let short = "still short";
        assert_eq!(trim_prompt_text("field notes", short), short);

        let long = format!("HEAD{}TAIL", "é".repeat(MAX_PROMPT_NOTES_BYTES));
        let out = trim_prompt_text("field notes", &long);
        assert!(out.starts_with("HEAD"), "the head is what survives");
        assert!(!out.contains("TAIL"));
        assert!(out.contains("field notes truncated"));
        // The marker names the section it cut, so a truncation notice under a
        // `## Directions` heading cannot say "field notes".
        assert!(trim_prompt_text("directions", &long).contains("directions truncated"));
        assert!(out.len() < MAX_PROMPT_NOTES_BYTES + 128);
        const { assert!(MAX_PROMPT_NOTES_BYTES < crate::protocol::MAX_NOTES_BYTES) };
    }

    /// A re-scout can carry both: a reviewed spec *and* leads from a run that
    /// never got as far as a verdict. History first, in that order.
    #[test]
    fn a_prompt_can_carry_both_a_review_and_field_notes() {
        let prior = reviewed("## Spec: old", Some("Say more."));
        let notes = salvaged("a later, interrupted look", None);
        let prompt = render_prompt(
            &task_fixture(),
            Some(&prior),
            Some(&notes),
            None,
            Duration::from_secs(3600),
        );

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
