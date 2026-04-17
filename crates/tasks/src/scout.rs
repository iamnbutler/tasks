//! Scout dispatcher: drives the Diamond 1 loop for a single task.
//!
//! Given a Task, the dispatcher allocates a VM from vm-pool, sends a
//! [`ScoutCommand::Start`], streams back [`ScoutEvent`]s, and persists the
//! resulting [`Spec`] + queue entry to the store. One scout at a time per
//! dispatcher instance (the vm-pool client holds a single event stream).

use std::sync::Arc;

use chrono::Utc;
use thiserror::Error;
use tracing::{info, warn};
use vm_pool_client::{Client, ClientError};
use vm_pool_protocol::{ServiceEvent, VmConfig, VmId};

use crate::events::EventPayload;
use crate::models::{
    Complexity, Session, SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry,
    SpecQueueStatus, Task, TaskState,
};
use crate::protocol::{ScoutCommand, ScoutEvent, TasksProtocol};
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub enum ScoutError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("vm-pool client: {0}")]
    Client(#[from] ClientError),
    #[error("scout failed: {0}")]
    ScoutFailed(String),
    #[error("vm-pool service error: {0}")]
    ServiceError(String),
    #[error("vm-pool event stream closed before completion")]
    StreamClosed,
}

/// How this dispatcher boots a Scout VM.
#[derive(Debug, Clone)]
pub struct ScoutConfig {
    /// Image reference to allocate from vm-pool, e.g. `"agent:v1"`.
    pub image: String,
    /// VM configuration passed to vm-pool.
    pub vm_config: VmConfig,
    /// Repo clone URL (what the scout-supervisor `git clone`s).
    pub repo_clone_url: String,
    /// Branch to base the throwaway scout branch on.
    pub base_branch: String,
}

pub struct Scout {
    store: Arc<Store>,
    client: Client<TasksProtocol>,
    config: ScoutConfig,
}

impl Scout {
    pub fn new(store: Arc<Store>, client: Client<TasksProtocol>, config: ScoutConfig) -> Self {
        Self {
            store,
            client,
            config,
        }
    }

    /// Dispatch a scout for `task`. Runs the full lifecycle: allocate VM,
    /// run scout, persist spec, deallocate VM.
    ///
    /// On success, returns the persisted [`Spec`]. Task state is advanced to
    /// `SpecReady` and a spec-queue entry is created with status
    /// `PendingReview`.
    pub async fn dispatch(&mut self, task: Task) -> Result<Spec, ScoutError> {
        info!(task_id = %task.id, "scout dispatch starting");

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
        let prompt = render_prompt(&task);

        // Allocate
        let vm_id = self.client.allocate(&self.config.image, self.config.vm_config.clone()).await?;
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
                ScoutCommand::Start {
                    task_id: task.id.to_string(),
                    repo_clone_url: self.config.repo_clone_url.clone(),
                    base_branch: self.config.base_branch.clone(),
                    prompt,
                },
            )
            .await
        {
            self.finalize_failed(&session_id, &task, &vm_id, format!("send: {e}"))
                .await?;
            return Err(e.into());
        }

        // Drain events until terminal Completed / Failed.
        let result = self.drain_scout_events(&vm_id).await;

        // Always try to deallocate. Ignore errors — the pool's health loop
        // will reap if we die mid-call.
        if let Err(e) = self.client.deallocate(&vm_id).await {
            warn!(%vm_id, error = %e, "failed to deallocate scout VM");
        }

        match result {
            Ok(DrainOutcome { branch, spec_markdown, files_touched, exit_code }) => {
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
                self.finalize_failed(&session_id, &task, &vm_id, reason).await?;
                Err(e)
            }
        }
    }

    async fn drain_scout_events(
        &mut self,
        target_vm: &VmId,
    ) -> Result<DrainOutcome, ScoutError> {
        let mut branch: Option<String> = None;
        let mut exit_code: Option<i32> = None;
        loop {
            let event = self
                .client
                .next_event()
                .await
                .ok_or(ScoutError::StreamClosed)?;

            match event {
                ServiceEvent::VmApp { vm_id, event: app } if &vm_id == target_vm => {
                    match app {
                        ScoutEvent::Started { branch: b } => {
                            branch = Some(b);
                        }
                        ScoutEvent::Progress { .. } => {
                            // Surface later via events/log tailing if useful.
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
                    }
                }
                ServiceEvent::VmApp { .. } => {
                    // Event for a different VM — ignore (one-scout-at-a-time invariant).
                }
                ServiceEvent::Error { message } => {
                    return Err(ScoutError::ServiceError(message));
                }
                _other => {
                    // CommandSent / PoolStatus / VmAllocated / etc. — not interesting here.
                }
            }
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
        sqlx_update_session_branch(&self.store, session_id, &branch).await?;
        sqlx_update_session_completion(
            &self.store,
            session_id,
            SessionStatus::ScoutSucceeded,
            now,
            None,
        )
        .await?;

        // Persist spec + queue entry
        let spec = Spec {
            id: SpecId::new(),
            session_id: session_id.clone(),
            task_id: task.id.clone(),
            content: spec_markdown,
            complexity: infer_complexity(&files_touched),
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
            .update_task_state(&task.id, TaskState::SpecReady)
            .await?;

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
            })
            .await?;
        self.store
            .append_event(EventPayload::TaskStateChanged {
                task_id: task.id.clone(),
                from: TaskState::Scouting,
                to: TaskState::SpecReady,
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
        sqlx_update_session_completion(
            &self.store,
            session_id,
            SessionStatus::ScoutFailed,
            now,
            Some(reason.clone()),
        )
        .await?;

        // Back to New so another scout can retry. The orchestrator enforces
        // the re-explore attempt cap.
        self.store
            .update_task_state(&task.id, TaskState::New)
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
                to: TaskState::New,
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

fn render_prompt(task: &Task) -> String {
    format!(
        "You are a Scout in the Double Diamond architecture.\n\n\
         ## Issue: {title} (#{num})\n\n\
         {body}\n\n\
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
    )
}

fn infer_complexity(files_touched: &[String]) -> Complexity {
    match files_touched.len() {
        0..=2 => Complexity::Simple,
        3..=8 => Complexity::Medium,
        _ => Complexity::Complex,
    }
}

// Thin helper wrappers so we keep the main flow readable. They bypass
// `Store`'s public API to tweak specific columns that aren't generally
// mutable; in the future either expose them or refactor.
async fn sqlx_update_session_branch(
    store: &Store,
    id: &SessionId,
    branch: &str,
) -> Result<(), StoreError> {
    store.update_session_branch(id, branch).await
}

async fn sqlx_update_session_completion(
    store: &Store,
    id: &SessionId,
    status: SessionStatus,
    completed_at: chrono::DateTime<Utc>,
    exit_reason: Option<String>,
) -> Result<(), StoreError> {
    store
        .update_session_completion(id, status, completed_at, exit_reason)
        .await
}
