//! Builder dispatcher: drives one Diamond 2 run — a batch of approved specs
//! into one branch and one PR.
//!
//! Given a claimed [`Build`], the dispatcher allocates a Builder VM, sends a
//! [`BuildCommand::Start`] whose prompt is the concatenated spec markdown
//! (and nothing Scout-code-derived — see [`render_prompt`], the barrier's
//! last mile), streams back [`BuildEvent`]s, and *lands the branch itself*:
//! the VM's commits arrive as a thin git bundle over the event stream, the
//! server unbundles them into a scratch repo, verifies the tip, and pushes
//! with its own credentials. No repo-write credential ever enters a VM, and
//! the PR (the system's only GitHub write) is opened server-side.
//!
//! `dispatch` finalizes the build on every exit path; all the fallible work
//! lives in `attempt`, so no `?` can leave a build `running`.
//!
//! Like a scout, a build is two halves — set the run up, then [`Builder::follow`]
//! it — so [`Builder::reattach`] can re-enter the second half for a build a
//! previous process started. A restart mid-build used to cost the whole
//! implementation *and* leave the branch homeless.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::Utc;
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, info, warn};
use vm_pool_client::{ClientError, ClientHandle};
use vm_pool_protocol::{VmConfig, VmId};

use crate::cancel::Bounded;
use crate::events::EventPayload;
use crate::github::{GhError, GitHubClient};
use crate::models::{Build, Project, RunKind, Spec, Task, TranscriptOwner, TranscriptStream};
use crate::protocol::{
    BuildCommand, BuildEvent, FailureClass, TaskCommand, TaskEvent, TasksProtocol,
};
use crate::reattach::AppEvents;
use crate::redact::{redact, redact_line};
use crate::store::{CancelRequest, Store, StoreError, Strike};
use crate::transcript::{TranscriptSink, spawn_transcript_writer, transcript_stream};

#[derive(Debug, Error)]
pub enum BuilderError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("vm-pool client: {0}")]
    Client(#[from] ClientError),
    #[error("github: {0}")]
    GitHub(#[from] GhError),
    /// The supervisor reported a terminal failure. `class` is *its* answer to
    /// whether the run judged the work, carried on the event itself — see
    /// [`FailureClass`].
    #[error("build failed: {reason}")]
    BuildFailed { reason: String, class: FailureClass },
    /// The build could not be picked up after a restart: no VM recorded, the
    /// pool no longer has it, and nothing terminal in the replay. The batch's
    /// specs stay approved and the queue is left claimable, exactly as
    /// reconciliation would have left them.
    #[error("build could not be resumed: {0}")]
    NotResumable(String),
    #[error("branch egress: {0}")]
    Egress(String),
    #[error("vm-pool event stream closed before completion")]
    StreamClosed,
    #[error("build timed out after {secs}s")]
    Timeout { secs: u64 },
    /// Somebody stopped the run on purpose. Carries the whole request: the
    /// actor and the rationale are what make a cancelled build distinguishable
    /// from a failed one when the row is read back, and
    /// [`Store::finalize_build_cancelled`] writes them into `exit_reason`.
    #[error("build cancelled by {}", .0.actor.as_str())]
    Cancelled(CancelRequest),
}

impl BuilderError {
    /// Whether this failure judged the work — the one decision point this
    /// dispatcher has, read by [`Builder::conclude`].
    ///
    /// `Egress` is `Transport`: it happens *after* an implementation exists,
    /// so nothing about it judged the work. Surfacing and charging are
    /// separable — the waive path appends a `Note` naming the class and the
    /// underlying error, so the failure stays exactly as visible, and what
    /// charging would add is only the strike. `Timeout` is charged for the
    /// reason a scout's is: the run had the entire budget.
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::BuildFailed { class, .. } => *class,
            Self::NotResumable(_) => FailureClass::Orphaned,
            Self::Cancelled(_) => FailureClass::Cancelled,
            // Egress is transport in the most literal sense: the agent ran to
            // completion, an implementation exists, and the push is what
            // failed. Nothing judged the work. Waiving the strike does not
            // hide the failure — the waive path appends a `Note` naming the
            // class and the underlying error, and since #891 the bundle is
            // preserved with the `git fetch` that recovers it. #873 is the
            // case: 102 turns, exit 0, rejected by `bundle tip … does not
            // match the reported head …`, and charged for it.
            Self::Egress(_) => FailureClass::Transport,
            Self::Store(_)
            | Self::Client(_)
            | Self::GitHub(_)
            | Self::StreamClosed
            | Self::Timeout { .. } => FailureClass::Verdict,
        }
    }
}

/// How this dispatcher boots a Builder VM.
#[derive(Debug, Clone)]
pub struct BuilderConfig {
    /// Image reference to allocate from vm-pool, e.g. `"builder:v1"`.
    pub image: String,
    pub vm_config: VmConfig,
    /// Wall-clock budget for one build, allocation included.
    pub timeout: Duration,
    /// Where per-build scratch repos live (removed after each build,
    /// success or failure).
    pub scratch_root: PathBuf,
}

pub struct Builder {
    store: Arc<Store>,
    client: ClientHandle<TasksProtocol>,
    github: Arc<GitHubClient>,
    config: BuilderConfig,
}

impl Builder {
    pub fn new(
        store: Arc<Store>,
        client: ClientHandle<TasksProtocol>,
        github: Arc<GitHubClient>,
        config: BuilderConfig,
    ) -> Self {
        Self {
            store,
            client,
            github,
            config,
        }
    }

    /// Run a claimed (`running`) build to a terminal state. Finalizes the
    /// build row on every path and returns the finished build.
    pub async fn dispatch(&self, build: Build, clone_url: &str) -> Result<Build, BuilderError> {
        info!(build_id = %build.id, branch = %build.branch, "build dispatch starting");
        self.conclude(&build, self.attempt(&build, clone_url).await)
            .await
    }

    /// Pick up a build a previous process left running.
    ///
    /// **This always concludes `build`** — including when it cannot be
    /// resumed. [`Store::reconcile_orphaned_work_except`] skips rows handed to
    /// a reattach, and a `running` build nobody concludes wedges the serial
    /// queue forever, which is strictly worse than an orphaned session.
    pub async fn reattach(&self, build: Build, clone_url: &str) -> Result<Build, BuilderError> {
        info!(build_id = %build.id, branch = %build.branch, "reattaching to a build");
        self.conclude(&build, self.resume(&build, clone_url).await)
            .await
    }

    /// Record the outcome. Failure is finalized here and nowhere else, so no
    /// `?` inside `attempt`/`resume` can leave a build `running`.
    async fn conclude(
        &self,
        build: &Build,
        outcome: Result<Build, BuilderError>,
    ) -> Result<Build, BuilderError> {
        match outcome {
            Ok(done) => Ok(done),
            // A deliberate stop by an accountable actor, so it is `cancelled`
            // rather than `failed` and costs the batch no attempt: the specs go
            // back to `approved` and their tasks to `ready_to_build`, ready for
            // whoever stopped it to decide what happens next.
            Err(BuilderError::Cancelled(request)) => {
                let reason = request.exit_reason();
                warn!(build_id = %build.id, reason, "build cancelled");
                self.store
                    .finalize_build_cancelled(&build.id, &reason)
                    .await?;
                Err(BuilderError::Cancelled(request))
            }
            Err(e) => {
                let reason = redact(&format!("{e}"));
                warn!(build_id = %build.id, reason, "build failed");
                // A failure that says nothing about the specs must not spend
                // one of their attempts — a dropped API connection, or a build
                // nobody could pick up after a restart. Three of those would
                // otherwise `blocked` a batch that has never actually failed
                // to build. One decision point, off the class the supervisor
                // stamped and never off `reason`.
                let class = e.failure_class();
                let strike = Strike::for_class(class);
                if let Some(waiver) = class.waiver_reason() {
                    self.note_waived_strike(build, class, waiver, &reason).await;
                }
                self.store
                    .finalize_build_failed_with(&build.id, &reason, strike)
                    .await?;
                Err(e)
            }
        }
    }

    /// Say, on the event log, that a failure cost the batch nothing.
    ///
    /// Without this the waiver is invisible: the build row reads `failed` and
    /// the specs' `build_attempts` simply do not move, which is
    /// indistinguishable from the cap having been switched off. Best-effort —
    /// a breadcrumb must never cost the finalization that follows it.
    async fn note_waived_strike(
        &self,
        build: &Build,
        class: FailureClass,
        waiver: &str,
        reason: &str,
    ) {
        if let Err(e) = self
            .store
            .append_event(EventPayload::Note {
                source: crate::run::DISPATCHER.into(),
                message: format!(
                    "build {} failed as {class}, so its specs keep their build attempts: \
                     {waiver} ({reason})",
                    build.id
                ),
            })
            .await
        {
            warn!(build_id = %build.id, error = %e, "recording a waived build strike failed");
        }
    }

    /// Everything that can fail on a fresh build. A `?` here lands in
    /// [`Builder::conclude`]'s failure finalization.
    async fn attempt(&self, build: &Build, clone_url: &str) -> Result<Build, BuilderError> {
        let started = Instant::now();
        // Subscribe before allocating so no event for our VM can be missed.
        let mut events = self.client.subscribe_events();

        let batch = self.load_batch(build).await?;
        let project = self.project(build).await?;
        let prompt = render_prompt(&batch);

        let vm_id = self
            .client
            .allocate(&self.config.image, self.config.vm_config.clone())
            .await?;
        info!(
            %vm_id,
            build_id = %build.id,
            cpus = ?self.config.vm_config.cpus,
            memory_mb = ?self.config.vm_config.memory_mb,
            "allocated builder VM"
        );
        self.store.set_build_vm(&build.id, vm_id.as_str()).await?;

        self.client
            .send_to_vm(
                &vm_id,
                TaskCommand::Build(BuildCommand::Start {
                    build_id: build.id.to_string(),
                    repo_clone_url: clone_url.to_string(),
                    base_branch: build.base_branch.clone(),
                    branch: build.branch.clone(),
                    prompt,
                }),
            )
            .await?;

        let remaining = self.config.timeout.saturating_sub(started.elapsed());
        let app = AppEvents::live(&mut events, vm_id.clone());
        self.follow(build, clone_url, &batch, &project, &vm_id, app, remaining)
            .await
    }

    /// Everything that can fail on a resumed build.
    async fn resume(&self, build: &Build, clone_url: &str) -> Result<Build, BuilderError> {
        let Some(vm_id) = build.vm_id.clone().map(VmId::new) else {
            return Err(BuilderError::NotResumable("the build records no VM".into()));
        };

        let batch = self.load_batch(build).await?;
        let project = self.project(build).await?;

        let (mut events, resume) = crate::reattach::attach(&self.client, &vm_id)
            .await
            .map_err(|e| BuilderError::NotResumable(format!("attach failed: {e}")))?;

        // Gone from the pool *and* silent is the only real orphan; a VM reaped
        // after the build finished still has its Completed — bundle included —
        // sitting in the replay, and that is a whole implementation.
        if !resume.present && !resume.replay.iter().any(is_terminal) {
            return Err(BuilderError::NotResumable(
                "the VM is gone and its build never reported an outcome".into(),
            ));
        }

        // Wall-clock from the original claim, floored so a replay already
        // holding the outcome is never thrown away by a spent budget.
        let elapsed = build
            .started_at
            .map(|t| (Utc::now() - t).to_std().unwrap_or_default())
            .unwrap_or_default();
        let remaining = self
            .config
            .timeout
            .saturating_sub(elapsed)
            .max(RESUME_MIN_BUDGET);

        let app = AppEvents::resumed(&mut events, vm_id.clone(), resume);
        self.follow(build, clone_url, &batch, &project, &vm_id, app, remaining)
            .await
    }

    /// The second half of a build, shared by [`Builder::attempt`] and
    /// [`Builder::resume`]: drain to a terminal event, deallocate, land the
    /// branch, open the PR.
    #[allow(clippy::too_many_arguments)]
    async fn follow(
        &self,
        build: &Build,
        clone_url: &str,
        batch: &[(Spec, Task)],
        project: &Project,
        vm_id: &VmId,
        mut events: AppEvents<'_>,
        budget: Duration,
    ) -> Result<Build, BuilderError> {
        let (mut sink, writer) =
            spawn_transcript_writer(self.store.clone(), TranscriptOwner::build(&build.id));
        // A cancel rides the same `select!` the deadline does — see
        // `crate::cancel`. Destroying the VM alone would leave this drain
        // parked on a stream that never speaks again, which is the bug (#876),
        // not the fix.
        let result = match crate::cancel::bounded(
            &self.store,
            RunKind::Build,
            build.id.as_str(),
            budget,
            self.drain_build_events(&mut events, &build.id, &mut sink),
        )
        .await
        {
            Bounded::Completed(result) => result,
            Bounded::Cancelled(request) => {
                warn!(
                    build_id = %build.id,
                    %vm_id,
                    actor = request.actor.as_str(),
                    "build cancelled; tearing the VM down"
                );
                Err(BuilderError::Cancelled(request))
            }
            Bounded::TimedOut => {
                // Said here rather than only in `dispatch`'s failure warn,
                // which runs *after* teardown: in the incident that was 15:37
                // to 17:50 of silence between the budget expiring and anyone
                // being told.
                warn!(
                    build_id = %build.id,
                    secs = self.config.timeout.as_secs(),
                    "build budget exhausted; tearing the VM down"
                );
                Err(BuilderError::Timeout {
                    secs: self.config.timeout.as_secs(),
                })
            }
        };

        // The agent phase ends here — before teardown, and long before the
        // push and the PR that `completed_at` waits for. Stamped on the
        // timeout path too, since that is exactly the duration someone will
        // want to read afterwards. Best-effort: a store hiccup must not skip
        // the deallocation below.
        //
        // Above the flush deliberately: draining a queued transcript is our
        // bookkeeping, not the agent's work, and an 8 MiB tail would otherwise
        // be charged to the interval the run budget bounds — the exact
        // conflation `agent_finished_at` exists to end.
        if let Err(e) = self
            .store
            .set_build_agent_finished(&build.id, Utc::now())
            .await
        {
            warn!(build_id = %build.id, error = %e, "could not stamp the agent phase end");
        }

        // Close the queue and let the writer finish *before* the build row is
        // finalized, so a client refetching on `build_completed` finds the
        // whole transcript rather than a truncated one. This has to happen
        // before `result?` escapes to `dispatch`'s failure finalization — and
        // on the timeout path especially, where the drain future was
        // cancelled and whatever is queued here is all that survives. The
        // silent failure this whole thing exists for is exactly that path.
        crate::transcript::flush(sink, writer, build.id.as_str()).await;

        crate::teardown::deallocate_bounded(
            &self.client,
            &self.store,
            vm_id,
            &format!("build {}", build.id),
            crate::teardown::DEALLOCATE_TIMEOUT,
        )
        .await;
        let outcome = result?;

        // Egress: unbundle, verify, push — then the PR.
        self.land_branch(build, clone_url, &outcome).await?;
        let (title, body) = pr_text(batch, &outcome);
        let pr_number = self
            .github
            .create_pull_request(
                &project.repo_owner,
                &project.repo_name,
                &build.branch,
                &build.base_branch,
                &title,
                &body,
            )
            .await?;
        info!(build_id = %build.id, pr_number, "pull request opened");

        Ok(self
            .store
            .finalize_build_succeeded(
                &build.id,
                &outcome.head_sha,
                pr_number,
                outcome.summary.as_deref(),
                &outcome.files_touched,
            )
            .await?)
    }

    async fn project(&self, build: &Build) -> Result<Project, BuilderError> {
        self.store
            .get_project(&build.project_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("project {}", build.project_id)).into())
    }

    /// Load the batch's `(Spec, Task)` pairs in position order.
    async fn load_batch(&self, build: &Build) -> Result<Vec<(Spec, Task)>, BuilderError> {
        let mut batch = Vec::new();
        for spec_id in self.store.build_spec_ids(&build.id).await? {
            let spec = self
                .store
                .get_spec(&spec_id)
                .await?
                .ok_or_else(|| StoreError::NotFound(format!("spec {spec_id}")))?;
            let task = self
                .store
                .get_task(&spec.task_id)
                .await?
                .ok_or_else(|| StoreError::NotFound(format!("task {}", spec.task_id)))?;
            batch.push((spec, task));
        }
        if batch.is_empty() {
            return Err(BuilderError::BuildFailed {
                reason: "build has no specs".into(),
                class: FailureClass::Verdict,
            });
        }
        Ok(batch)
    }

    /// Consume this run's events — the replay first if there is one, then
    /// live — until its VM reports a terminal Completed/Failed, recording
    /// every line the agent emits into `sink` on the way past.
    ///
    /// Nothing here is append-only, so a replayed event needs no special
    /// handling: `base_sha` is an idempotent write and `Progress` is only
    /// logged. The one event that matters is `Completed`, and the bounded
    /// replay keeps the newest events precisely so it survives.
    async fn drain_build_events(
        &self,
        events: &mut AppEvents<'_>,
        build_id: &crate::models::BuildId,
        sink: &mut TranscriptSink,
    ) -> Result<BuildOutcome, BuilderError> {
        loop {
            let (_origin, event) = events.next().await.ok_or(BuilderError::StreamClosed)?;
            match event {
                TaskEvent::Build(app) => match app {
                    BuildEvent::Started { base_sha } => {
                        self.store.set_build_base_sha(build_id, &base_sha).await?;
                    }
                    BuildEvent::Progress { stream, line } => {
                        // Two sinks, two scrubs. The log is redacted here; the
                        // persisted copy is redacted inside `TranscriptSink::push`,
                        // which #825 made the shared path for both owners — git
                        // echoes the credentialed clone URL its VM was handed,
                        // and a log file is a file.
                        debug!(build_id = %build_id, "{}", redact_line(&line));
                        sink.push(transcript_stream(stream), line);
                    }
                    BuildEvent::ImplementationFinished { exit_code } => {
                        info!(build_id = %build_id, exit_code, "builder agent finished");
                        // The server's own line, in the agent's stream: an
                        // exit code buried in a log file is not in the same
                        // ordered record as the output that explains it.
                        sink.push(
                            TranscriptStream::Stderr,
                            format!("[tasks] builder agent exited with code {exit_code}"),
                        );
                    }
                    BuildEvent::Completed {
                        base_sha,
                        head_sha,
                        bundle_base64,
                        summary,
                        files_touched,
                    } => {
                        return Ok(BuildOutcome {
                            base_sha,
                            head_sha,
                            bundle_base64,
                            summary,
                            files_touched,
                        });
                    }
                    BuildEvent::Failed { reason, class } => {
                        return Err(BuilderError::BuildFailed { reason, class });
                    }
                },
                // A Scout VM's traffic on the same connection. Not ours.
                TaskEvent::Scout(_) => {}
            }
        }
    }

    /// Unbundle the VM's commits into a per-build scratch repo, verify the
    /// tip matches what the VM reported, and push the branch with the
    /// server's credentials.
    ///
    /// The scratch repo fetches the base branch from the remote FIRST: the
    /// bundle is thin, `base_sha` is its prerequisite, and the remote is
    /// where that commit stays reachable even if the base branch has moved
    /// on. Fetching it shallowly would reintroduce the shallow-repo problem
    /// one layer down.
    /// Every `Egress` failure routes through [`Builder::preserve_bundle`]: the
    /// VM was deallocated before this ran, so the bundle in `outcome` is the
    /// only copy of the implementation left anywhere.
    async fn land_branch(
        &self,
        build: &Build,
        clone_url: &str,
        outcome: &BuildOutcome,
    ) -> Result<(), BuilderError> {
        match self.land_and_sweep(build, clone_url, outcome).await {
            // The variant is unwrapped and re-wrapped, so the reason reads
            // `branch egress: …` once rather than twice.
            Err(BuilderError::Egress(why)) => Err(self.preserve_bundle(build, outcome, &why).await),
            other => other,
        }
    }

    async fn land_and_sweep(
        &self,
        build: &Build,
        clone_url: &str,
        outcome: &BuildOutcome,
    ) -> Result<(), BuilderError> {
        let scratch = self
            .config
            .scratch_root
            .join(format!("scratch-{}", build.id));
        tokio::fs::create_dir_all(&scratch)
            .await
            .map_err(|e| BuilderError::Egress(format!("scratch dir: {e}")))?;

        let result = self.land_in(&scratch, build, clone_url, outcome).await;
        if let Err(e) = tokio::fs::remove_dir_all(&scratch).await {
            warn!(scratch = %scratch.display(), error = %e, "could not remove scratch repo");
        }
        result
    }

    /// Write a rejected bundle down, and name the command that recovers it.
    ///
    /// [`Builder::follow`] tears the VM down *before* egress runs — deliberately,
    /// since holding a VM across a push and a PR is a worse trade — so an
    /// egress failure used to destroy the whole implementation with nothing
    /// left to recover it from. Best-effort: a failure to preserve is appended
    /// to the reason rather than replacing it, because the original failure is
    /// still the thing that went wrong.
    async fn preserve_bundle(
        &self,
        build: &Build,
        outcome: &BuildOutcome,
        why: &str,
    ) -> BuilderError {
        let dir = self.config.scratch_root.join(REJECTED_BUNDLE_DIR);
        let path = dir.join(format!("{}.bundle", build.id));

        let saved = async {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&outcome.bundle_base64)
                .map_err(|e| format!("bundle base64: {e}"))?;
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| format!("rejected bundle dir: {e}"))?;
            tokio::fs::write(&path, bytes)
                .await
                .map_err(|e| format!("rejected bundle write: {e}"))?;
            Ok::<(), String>(())
        }
        .await;

        BuilderError::Egress(match saved {
            Ok(()) => {
                warn!(
                    build_id = %build.id,
                    bundle = %path.display(),
                    "egress failed; the build's commits were preserved"
                );
                format!(
                    "{why}; the build's commits were kept at {bundle} — recover them with \
                     git fetch {bundle} '{branch}:{branch}'",
                    bundle = path.display(),
                    branch = build.branch,
                )
            }
            Err(e) => format!("{why}; the build's commits could not be preserved either: {e}"),
        })
    }

    async fn land_in(
        &self,
        scratch: &std::path::Path,
        build: &Build,
        clone_url: &str,
        outcome: &BuildOutcome,
    ) -> Result<(), BuilderError> {
        let branch_ref = format!("refs/heads/{}", build.branch);
        let base_ref = format!("refs/heads/{}", build.base_branch);

        git(scratch, &["init", "--bare", "--initial-branch", "trunk"]).await?;
        git(
            scratch,
            &["fetch", clone_url, &format!("{base_ref}:{base_ref}")],
        )
        .await?;

        let bundle_bytes = base64::engine::general_purpose::STANDARD
            .decode(&outcome.bundle_base64)
            .map_err(|e| BuilderError::Egress(format!("bundle base64: {e}")))?;
        let bundle_path = scratch.join("egress.bundle");
        tokio::fs::write(&bundle_path, bundle_bytes)
            .await
            .map_err(|e| BuilderError::Egress(format!("bundle write: {e}")))?;

        git(
            scratch,
            &[
                "fetch",
                bundle_path.to_str().unwrap_or_default(),
                &format!("{branch_ref}:{branch_ref}"),
            ],
        )
        .await?;

        // Verify the tip before pushing: a truncated or wrong bundle must not
        // be pushed as if it were the build. Since #891 the VM reads
        // `head_sha` back out of the bundle it packaged rather than observing
        // it a second time in the worktree, so this no longer races the VM —
        // it compares the bundle as sent with the bundle as received, which is
        // transport integrity and nothing else.
        let tip = git_stdout(scratch, &["rev-parse", &branch_ref]).await?;
        if tip != outcome.head_sha {
            return Err(BuilderError::Egress(format!(
                "bundle tip {tip} does not match the reported head {}",
                outcome.head_sha
            )));
        }

        git(
            scratch,
            &["push", clone_url, &format!("{branch_ref}:{branch_ref}")],
        )
        .await?;
        info!(build_id = %build.id, branch = %build.branch, "branch pushed");
        Ok(())
    }
}

/// Where a bundle that could not be pushed is written, under
/// [`BuilderConfig::scratch_root`] and deliberately **outside** the per-build
/// scratch repo — that repo is still removed after every build, this is not.
/// Its contents are whole implementations, so removing one is a human's call,
/// which also makes it unbounded by design.
const REJECTED_BUNDLE_DIR: &str = "rejected";

/// Least wall-clock budget a resumed build gets, however long the server was
/// down. See `scout::RESUME_MIN_BUDGET` — same reasoning, and here the
/// outcome in hand is a whole implementation plus its bundle.
const RESUME_MIN_BUDGET: Duration = Duration::from_secs(30);

/// Whether a build event ends the run.
fn is_terminal(event: &TaskEvent) -> bool {
    matches!(
        event,
        TaskEvent::Build(BuildEvent::Completed { .. } | BuildEvent::Failed { .. })
    )
}

struct BuildOutcome {
    #[allow(dead_code)] // verified VM-side; kept for symmetry / debugging
    base_sha: String,
    head_sha: String,
    bundle_base64: String,
    summary: Option<String>,
    files_touched: Vec<String>,
}

async fn git(dir: &std::path::Path, args: &[&str]) -> Result<(), BuilderError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| BuilderError::Egress(format!("spawn git: {e}")))?;
    if !output.status.success() {
        // git echoes URLs (credentials included) in most errors.
        return Err(BuilderError::Egress(redact(&format!(
            "git {} exited with {}: {}",
            args.first().unwrap_or(&""),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))));
    }
    Ok(())
}

async fn git_stdout(dir: &std::path::Path, args: &[&str]) -> Result<String, BuilderError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| BuilderError::Egress(format!("spawn git: {e}")))?;
    if !output.status.success() {
        return Err(BuilderError::Egress(redact(&format!(
            "git {} exited with {}: {}",
            args.first().unwrap_or(&""),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The Builder prompt: concatenated spec markdown plus issue title/number.
/// This function is the information barrier's last mile — it takes `(Spec,
/// Task)` pairs and emits nothing Scout-code-derived. If anything else ever
/// needs to reach a Builder, the argument has to be made here, and the answer
/// should be no.
fn render_prompt(batch: &[(Spec, Task)]) -> String {
    let n = batch.len();
    let mut out = format!(
        "You are a Builder in the Double Diamond architecture.\n\n\
         You are implementing {n} approved spec(s). Each was written by a Scout \
         that already explored the work by implementing it once in a throwaway \
         branch you cannot see — the spec is the distilled result. Trust its \
         pitfalls; verify its claims against the code in front of you.\n\n"
    );
    for (i, (spec, task)) in batch.iter().enumerate() {
        out.push_str(&format!(
            "## Spec {idx} of {n}: {title} (#{num})\n\n{content}\n\n",
            idx = i + 1,
            title = task.title,
            num = task.gh_issue_number,
            content = spec.content.trim(),
        ));
    }
    out.push_str(
        "## Your job\n\n\
         1. Implement every spec above, in order, as one coherent change in \
         the cloned repo (cwd). You are on the right branch already.\n\
         2. Run the project's tests / lint / typecheck — get them green.\n\
         3. Commit your work with clear messages (a git identity is configured).\n\
         4. Write `SUMMARY.md` in the repo root: one or two paragraphs \
         describing the change, suitable as a pull request body. Do not use \
         GitHub closing keywords (`Closes #N`, `Fixes #N`) — the server \
         links the issues itself.\n\
         5. End `SUMMARY.md` with one line saying whether you actually ran the \
         tests, in exactly this shape:\n\
         `Verification: PASSED — <the command you ran>`\n\
         `Verification: FAILED — <the command, and what failed>`\n\
         `Verification: NOT RUN — <why not>`\n\
         Report what actually happened. Nothing re-runs this suite for you \
         downstream, so this line is the only evidence anyone has that the \
         change works — claiming a run you did not make is the one thing here \
         that cannot be caught later, and \"NOT RUN\" costs the batch a look \
         from a human rather than costing you anything.\n\
         6. Do NOT push and do NOT open a PR — the server does both.\n",
    );
    out
}

/// The marker a Builder's `SUMMARY.md` carries its test-run claim under.
///
/// A trailer in the summary, rather than a column or a protocol field, for one
/// reason worth stating: the summary is *already* stored and *already* the PR
/// body, so one sentence serves the human reading the PR on GitHub and the
/// brief reading it back, with no migration, no `BuildEvent` field and no
/// builder-image rebuild in between. It also degrades correctly on rows that
/// predate it — they parse as [`VerificationReport::Unreported`].
pub const VERIFICATION_PREFIX: &str = "Verification:";

/// Detail longer than this is truncated. One line, bounded: this ends up in a
/// brief, whose whole value is being cheaper to read than the thing it summarizes.
const MAX_VERIFICATION_DETAIL: usize = 200;

/// What a build claimed about its own test run.
///
/// A *claim*, never a check — nothing here re-runs anything. The point of
/// keeping [`Self::Unreported`] separate from [`Self::NotRun`] is that they
/// mean different things: one build said it skipped the tests, the other said
/// nothing at all, and only the second is compatible with "the tests passed
/// and the line was forgotten".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationReport {
    Passed(String),
    Failed(String),
    NotRun(String),
    Unreported,
}

/// Read the verification trailer out of a `SUMMARY.md`.
///
/// Scans every line because agents append trailers, takes the first marker it
/// recognizes, and is deliberately forgiving about the shape agents actually
/// produce — bullets, casing, `—`/`-`/`:` between the marker and the detail.
/// Anything it cannot recognize is [`VerificationReport::Unreported`] and
/// **never** a pass: the direction a mistake here has to fall is towards a
/// human looking at it.
pub fn verification_report(summary: Option<&str>) -> VerificationReport {
    let Some(summary) = summary else {
        return VerificationReport::Unreported;
    };
    for line in summary.lines() {
        // Bullets and emphasis around the trailer: `- **Verification:** …`.
        let line = line.trim().trim_start_matches(['-', '*', '#', ' ']).trim();
        let Some(rest) = strip_prefix_ci(line, VERIFICATION_PREFIX) else {
            continue;
        };
        // `**Verification:** …` leaves the closing emphasis behind.
        let rest = rest.trim_start_matches(['*', ' ']).trim();
        for (marker, build) in [
            ("PASSED", VerificationReport::Passed as fn(String) -> _),
            ("FAILED", VerificationReport::Failed as fn(String) -> _),
            ("NOT RUN", VerificationReport::NotRun as fn(String) -> _),
            ("NOT_RUN", VerificationReport::NotRun as fn(String) -> _),
        ] {
            let Some(detail) = strip_prefix_ci(rest, marker) else {
                continue;
            };
            let detail = detail
                .trim_start_matches(['—', '–', '-', ':', ' '])
                .trim()
                .chars()
                .take(MAX_VERIFICATION_DETAIL)
                .collect::<String>();
            return build(detail);
        }
    }
    VerificationReport::Unreported
}

/// `strip_prefix`, ASCII-case-insensitively.
fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    let head = haystack.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &haystack[prefix.len()..])
}

/// PR title + body. Body prefers the agent's SUMMARY.md; falls back to the
/// spec titles. Says `Implements #N`, not `Closes #N`: closing an issue is
/// GitHub state that isn't ours to write.
fn pr_text(batch: &[(Spec, Task)], outcome: &BuildOutcome) -> (String, String) {
    let title = match batch {
        [(_, task)] => task.title.clone(),
        _ => format!(
            "Build: {}",
            batch
                .iter()
                .map(|(_, t)| format!("#{}", t.gh_issue_number))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let mut body = match &outcome.summary {
        Some(s) => neutralize_closing_keywords(s),
        None => batch
            .iter()
            .map(|(_, t)| format!("- {} (#{})", t.title, t.gh_issue_number))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    body.push_str("\n\n");
    for (_, task) in batch {
        body.push_str(&format!("Implements #{}\n", task.gh_issue_number));
    }
    (title, body)
}

/// Rewrite GitHub closing keywords (`Closes #N`, `Fixes #N`, …) in
/// agent-authored text to `Implements #N`. GitHub reads those keywords
/// anywhere in a PR body, so an agent writing "Closes #763" in SUMMARY.md
/// would make the merge close the issue — GitHub state that isn't ours to
/// write. The keyword only counts when a `#<digits>` reference follows.
fn neutralize_closing_keywords(text: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if !c.is_ascii_alphabetic() {
            out.push(c);
            prev = Some(c);
            chars.next();
            continue;
        }
        let mut end = start;
        while let Some(&(i, ch)) = chars.peek() {
            if !ch.is_ascii_alphabetic() {
                break;
            }
            end = i + ch.len_utf8();
            chars.next();
        }
        let word = &text[start..end];
        // A word boundary on the left (maximal alpha run handles letters;
        // this rules out things like "v2fixed") …
        let standalone = !prev.is_some_and(|p| p.is_ascii_alphanumeric());
        // … a closing keyword, and an issue reference on the right — GitHub
        // accepts an optional colon between them.
        let closes_a_ref = standalone && KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k)) && {
            let rest = text[end..].strip_prefix(':').unwrap_or(&text[end..]);
            let rest = rest.trim_start();
            rest.strip_prefix('#')
                .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
        };
        out.push_str(if closes_a_ref { "Implements" } else { word });
        prev = word.chars().last();
    }
    out
}

/// The clone URL a Builder VM uses — same construction as scouts.
pub fn project_clone_url(base: &str, token: Option<&str>, project: &Project) -> String {
    crate::run::clone_url_for(base, token, project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Complexity, GhState, ProjectId, SessionId, SpecId, TaskId, TaskState};
    use chrono::Utc;

    fn pair(n: u64, title: &str, content: &str) -> (Spec, Task) {
        let task_id = TaskId::new();
        (
            Spec {
                id: SpecId::new(),
                session_id: Some(SessionId::new()),
                task_id: task_id.clone(),
                content: content.into(),
                complexity: Complexity::Simple,
                files_touched: vec!["secret_scout_file.rs".into()],
                created_at: Utc::now(),
            },
            Task {
                id: task_id,
                project_id: ProjectId::new(),
                gh_issue_number: n,
                title: title.into(),
                body: "issue body".into(),
                labels: vec![],
                gh_state: GhState::Open,
                state: TaskState::Building,
                priority: 0,
                manual_rank: None,
                dispatch_attempts: 0,
                ingested_at: Utc::now(),
                updated_at: Utc::now(),
            },
        )
    }

    #[test]
    fn a_failed_egress_is_transport_and_a_concluded_build_is_a_verdict() {
        // #873 ran 102 turns, exited 0, and was rejected by the tip check.
        // Nothing judged that implementation, so it must not spend a strike;
        // the negative half is what keeps this from reading as "cap off".
        assert_eq!(
            BuilderError::Egress("bundle tip does not match".into()).failure_class(),
            FailureClass::Transport,
        );
        assert!(
            !BuilderError::Egress("x".into())
                .failure_class()
                .is_verdict()
        );

        assert_eq!(
            BuilderError::BuildFailed {
                reason: "agent produced no commits".into(),
                class: FailureClass::Verdict,
            }
            .failure_class(),
            FailureClass::Verdict,
        );
        assert_eq!(
            BuilderError::Timeout { secs: 1 }.failure_class(),
            FailureClass::Verdict,
        );
    }

    #[test]
    fn the_prompt_is_specs_and_issue_identity_only() {
        let batch = vec![
            pair(7, "First thing", "## Spec: first\ndo it"),
            pair(9, "Second thing", "## Spec: second\ndo that"),
        ];
        let prompt = render_prompt(&batch);

        let first = prompt.find("## Spec 1 of 2: First thing (#7)").unwrap();
        let second = prompt.find("## Spec 2 of 2: Second thing (#9)").unwrap();
        let job = prompt.find("## Your job").unwrap();
        assert!(first < second && second < job, "batch order preserved");
        assert!(prompt.contains("do it") && prompt.contains("do that"));

        // The barrier: nothing scout-run-derived leaks — not the files the
        // scout touched, not the issue body (the spec subsumes it).
        assert!(!prompt.contains("secret_scout_file.rs"));
        assert!(!prompt.contains("issue body"));
    }

    /// The Builder's own test run is the only evidence this repository can
    /// produce that a change works — there are no workflows and no required
    /// checks — so the prompt has to ask for it, and has to ask for it
    /// *truthfully*. A line that agents learn to write unconditionally is
    /// worse than no line, because the brief reads it back as evidence.
    #[test]
    fn the_prompt_asks_for_the_verification_line_and_for_the_truth() {
        let prompt = render_prompt(&[pair(7, "A thing", "spec")]);
        assert!(prompt.contains("Verification: PASSED"), "{prompt}");
        assert!(prompt.contains("Verification: FAILED"), "{prompt}");
        assert!(prompt.contains("Verification: NOT RUN"), "{prompt}");
        assert!(prompt.contains("Report what actually happened"), "{prompt}");
        assert!(
            prompt.contains("cannot be caught later"),
            "the reason the line has to be honest: {prompt}"
        );
        // The step it displaced is still there, renumbered.
        assert!(prompt.contains("6. Do NOT push"), "{prompt}");
    }

    /// The parser meets agents where they write. What it must never do is
    /// promote something it did not understand into a pass.
    #[test]
    fn the_verification_trailer_survives_the_shapes_agents_write() {
        let report = |s: &str| verification_report(Some(s));

        assert_eq!(
            report("Did the thing.\n\nVerification: PASSED — make test (579 tests)"),
            VerificationReport::Passed("make test (579 tests)".into())
        );
        // Case, bullets and emphasis.
        assert_eq!(
            report("- **verification:** passed - cargo test"),
            VerificationReport::Passed("cargo test".into())
        );
        assert_eq!(
            report("* Verification: FAILED: make test, 2 store tests red"),
            VerificationReport::Failed("make test, 2 store tests red".into())
        );
        assert_eq!(
            report("Verification: not run — the suite needs a display"),
            VerificationReport::NotRun("the suite needs a display".into())
        );
        assert_eq!(
            report("Verification: NOT_RUN — no test runner in the image"),
            VerificationReport::NotRun("no test runner in the image".into())
        );
        // The first marker wins; a summary that argues with itself is not a
        // reason to search for the most favourable line.
        assert_eq!(
            report("Verification: FAILED — one test red\nVerification: PASSED — later"),
            VerificationReport::Failed("one test red".into())
        );

        // Everything unrecognized, including a build that predates the line.
        assert_eq!(verification_report(None), VerificationReport::Unreported);
        assert_eq!(
            report("Just prose about the change."),
            VerificationReport::Unreported
        );
        assert_eq!(
            report("Verification: probably fine"),
            VerificationReport::Unreported
        );
        assert_eq!(
            report("The tests passed."),
            VerificationReport::Unreported,
            "prose about passing is not the trailer"
        );
    }

    #[test]
    fn the_verification_detail_is_one_bounded_line() {
        let long = format!("Verification: PASSED — {}\nmore prose", "x".repeat(500));
        match verification_report(Some(&long)) {
            VerificationReport::Passed(detail) => {
                assert_eq!(detail.chars().count(), MAX_VERIFICATION_DETAIL);
                assert!(!detail.contains("more prose"));
            }
            other => panic!("expected a pass, got {other:?}"),
        }
    }

    #[test]
    fn pr_text_prefers_the_summary_and_never_says_closes() {
        let batch = vec![pair(7, "First thing", "spec"), pair(9, "Second", "spec")];
        let outcome = BuildOutcome {
            base_sha: "a".into(),
            head_sha: "b".into(),
            bundle_base64: String::new(),
            summary: Some("Did both things.".into()),
            files_touched: vec![],
        };
        let (title, body) = pr_text(&batch, &outcome);
        assert_eq!(title, "Build: #7, #9");
        assert!(body.starts_with("Did both things."));
        assert!(body.contains("Implements #7"));
        assert!(body.contains("Implements #9"));
        assert!(!body.contains("Closes"));

        let single = vec![pair(7, "First thing", "spec")];
        let no_summary = BuildOutcome {
            summary: None,
            ..outcome
        };
        let (title, body) = pr_text(&single, &no_summary);
        assert_eq!(title, "First thing");
        assert!(body.contains("- First thing (#7)"));
    }

    /// The live bug this guards: the #763 builder's SUMMARY.md began with
    /// "Closes #763.", which flowed verbatim into PR #785's body — merging
    /// would have GitHub close the issue on the server's behalf.
    #[test]
    fn agent_summaries_cannot_smuggle_closing_keywords() {
        assert_eq!(
            neutralize_closing_keywords("Closes #763.\n\nThe change adds fixtures."),
            "Implements #763.\n\nThe change adds fixtures."
        );
        // Every keyword GitHub honors, any case, optional colon.
        assert_eq!(neutralize_closing_keywords("fixes #12"), "Implements #12");
        assert_eq!(
            neutralize_closing_keywords("RESOLVED: #9"),
            "Implements: #9"
        );
        // A keyword without an issue reference is ordinary prose.
        assert_eq!(
            neutralize_closing_keywords("This fixes the flaky test and closes a gap."),
            "This fixes the flaky test and closes a gap."
        );
        // Part of a longer word is not a keyword.
        assert_eq!(
            neutralize_closing_keywords("unfixed #4 preCloses #5"),
            "unfixed #4 preCloses #5"
        );
        // A `#` not followed by digits is not an issue reference.
        assert_eq!(neutralize_closing_keywords("fixes #abc"), "fixes #abc");

        let batch = vec![pair(763, "Golden fixtures", "spec")];
        let outcome = BuildOutcome {
            base_sha: "a".into(),
            head_sha: "b".into(),
            bundle_base64: String::new(),
            summary: Some("Closes #763. Adds golden fixtures.".into()),
            files_touched: vec![],
        };
        let (_, body) = pr_text(&batch, &outcome);
        assert!(!body.contains("Closes"));
        assert!(body.starts_with("Implements #763. Adds golden fixtures."));
    }
}
