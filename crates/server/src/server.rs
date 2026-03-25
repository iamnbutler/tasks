//! Server — the platform, spec Section 3.1.
//!
//! The long-running process that hosts the event log, task state,
//! merge queue, scheduler, and serves the web GUI.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;
use tokio::sync::RwLock;

use events::{Actor, Event, EventBus, EventType};

use crate::merge_queue::MergeQueue;
use crate::model::merge_queue::MergeStatus;
use crate::mode::{Mode, ModeTransitionError};
use crate::model::automation::{Automation, AutomationRun, AutomationState, TriggerType};
use crate::model::project::Project;
use crate::model::task::{FailureInfo, Task, TaskSource, TaskState};
use crate::dispatcher::{self, DispatchPlan};
use crate::presence::PresenceTracker;

/// Internal action type for merge queue reconciliation.
/// Used to decide what to do outside the read lock.
enum MqAction {
    MarkMerged { entry_id: String, task_id: String, pr_url: String },
    Remove { entry_id: String, pr_url: String },
    MarkConflict { entry_id: String, pr_url: String },
    ClearConflict { entry_id: String },
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("mode transition: {0}")]
    ModeTransition(#[from] ModeTransitionError),
    #[error("event store: {0}")]
    EventStore(#[from] events::StoreError),
    #[error("project not found: {0}")]
    ProjectNotFound(String),
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error("store error: {0}")]
    StoreError(String),
    #[error("invalid operation: {0}")]
    InvalidOperation(String),
}

/// Statistics from a rebuild operation (issue #256).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebuildStats {
    /// Number of tasks cleared from memory/database.
    pub tasks_cleared: usize,
    /// Number of merge queue entries cleared from memory/database.
    pub merge_entries_cleared: usize,
}

/// Shared server state.
///
/// This is the core of the platform. All subsystems operate on this state.
pub struct ServerState {
    /// Current operating mode (spec Section 6).
    pub mode: Mode,
    /// All managed projects (spec Section 3.3).
    pub projects: HashMap<String, Project>,
    /// All tasks indexed by ID.
    pub tasks: HashMap<String, Task>,
    /// The merge queue (spec Section 7).
    pub merge_queue: MergeQueue,
    /// All automations indexed by ID (spec Section 5.7).
    pub automations: HashMap<String, Automation>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            mode: Mode::Pause,
            projects: HashMap::new(),
            tasks: HashMap::new(),
            merge_queue: MergeQueue::new(),
            automations: HashMap::new(),
        }
    }
}

/// The Tasks server.
///
/// Spec Section 3.1: "The server is the platform."
///
/// Hosts:
/// - Event log (via EventBus)
/// - Task state
/// - Merge queue
/// - Operating mode
/// - Human presence tracking
/// - Scheduler (Section 12: GitHub sync, issue/PR → task import)
/// - Dispatcher (Section 12.6: priority-based dispatch with concurrency limits)
/// - Workflow configuration (Section 10: project/label/prompt config)
/// - Prompt construction (Section 11: system prompt + task context assembly)
///
/// Not yet implemented (spec TODOs):
/// - Web GUI serving (Section 3.1)
/// - Websocket connections (Section 3.1)
/// - Orchestrator integration (Section 4.2)
pub struct Server {
    pub state: Arc<RwLock<ServerState>>,
    pub event_bus: Arc<EventBus>,
    pub presence: Arc<PresenceTracker>,
    pub(crate) store: Option<Arc<StdMutex<tasks_store::Store>>>,
    /// Flag to signal the poll loop to reset pollers (issue #256).
    rebuild_requested: AtomicBool,
}

impl Server {
    pub fn new(event_bus: EventBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            event_bus: Arc::new(event_bus),
            presence: Arc::new(PresenceTracker::new()),
            store: None,
            rebuild_requested: AtomicBool::new(false),
        }
    }

    /// Create a server with persistent storage (spec Section 3.5).
    pub fn with_store(event_bus: EventBus, store: tasks_store::Store) -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            event_bus: Arc::new(event_bus),
            presence: Arc::new(PresenceTracker::new()),
            store: Some(Arc::new(StdMutex::new(store))),
            rebuild_requested: AtomicBool::new(false),
        }
    }

    /// Load projects, tasks, and merge queue entries from the store into memory.
    /// Called once at startup.
    pub async fn load_from_store(&self) -> Result<(), ServerError> {
        let store = match &self.store {
            Some(s) => s,
            None => return Ok(()),
        };

        let store = store.lock().unwrap();
        let mut state = self.state.write().await;

        // Load projects
        let projects = store
            .list_projects()
            .map_err(|e| ServerError::StoreError(e.to_string()))?;
        for project in projects {
            state.projects.insert(project.id.clone(), project);
        }

        // Load tasks
        let tasks = store
            .list_tasks()
            .map_err(|e| ServerError::StoreError(e.to_string()))?;
        for task in tasks {
            state.tasks.insert(task.id.clone(), task);
        }

        // Load merge queue entries
        let entries = store
            .list_merge_entries()
            .map_err(|e| ServerError::StoreError(e.to_string()))?;
        for entry in entries {
            state.merge_queue.enqueue(entry);
        }

        // Load automations
        let automations = store
            .list_automations()
            .map_err(|e| ServerError::StoreError(e.to_string()))?;
        for automation in automations {
            state.automations.insert(automation.id.clone(), automation);
        }

        Ok(())
    }

    // --- Project management (spec Section 3.3) ---

    pub async fn add_project(&self, project: Project) {
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_project(&project) {
                    tracing::error!(project_id = %project.id, error = %e, "failed to persist project to store");
                }
            }
        }
        let mut state = self.state.write().await;
        state.projects.insert(project.id.clone(), project);
    }

    pub async fn remove_project(&self, id: &str) -> bool {
        let mut state = self.state.write().await;
        let removed = state.projects.remove(id).is_some();
        if removed {
            // Collect task IDs for this project
            let task_ids: Vec<String> = state
                .tasks
                .iter()
                .filter(|(_, task)| task.project == id)
                .map(|(task_id, _)| task_id.clone())
                .collect();

            // Remove tasks from in-memory state
            for task_id in &task_ids {
                state.tasks.remove(task_id);
            }

            // Remove merge queue entries for these tasks from in-memory state
            if !task_ids.is_empty() {
                state.merge_queue.remove_by_task_ids(&task_ids);
            }

            // Remove automations for this project from in-memory state
            let automation_ids: Vec<String> = state
                .automations
                .iter()
                .filter(|(_, automation)| automation.project_id == id)
                .map(|(automation_id, _)| automation_id.clone())
                .collect();
            for automation_id in &automation_ids {
                state.automations.remove(automation_id);
            }

            // Cascade delete in persistent store (transactional)
            if let Some(ref store) = self.store {
                if let Ok(store) = store.lock() {
                    if let Err(e) = store.delete_project_data(id, &task_ids) {
                        tracing::error!(project_id = %id, error = %e, "failed to cascade-delete project data from store");
                    }
                    if let Err(e) = store.delete_automations_for_project(id) {
                        tracing::error!(project_id = %id, error = %e, "failed to cascade-delete automations from store");
                    }
                    if let Err(e) = store.delete_project(id) {
                        tracing::error!(project_id = %id, error = %e, "failed to delete project from store");
                    }
                }
            }

            tracing::info!(
                project_id = %id,
                tasks_removed = task_ids.len(),
                automations_removed = automation_ids.len(),
                "project removed with cascading cleanup"
            );
        }
        removed
    }

    pub async fn get_project(&self, id: &str) -> Option<Project> {
        let state = self.state.read().await;
        state.projects.get(id).cloned()
    }

    /// Get the last polled timestamp for a project (poller high-water mark).
    ///
    /// Used to initialize the poller after server restarts (spec github.md §5.3).
    pub fn get_last_polled_at(&self, id: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.get_last_polled_at(id)
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    /// Set the last polled timestamp for a project (poller high-water mark).
    ///
    /// Called after each successful poll to persist the high-water mark.
    pub fn set_last_polled_at(&self, id: &str, timestamp: chrono::DateTime<chrono::Utc>) -> Result<(), ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.set_last_polled_at(id, timestamp)
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    // --- Automation management (spec Section 5.7) ---

    /// Add a new automation.
    pub async fn add_automation(&self, automation: Automation) -> Result<(), ServerError> {
        let automation_id = automation.id.clone();
        let project_id = automation.project_id.clone();

        // Verify project exists
        {
            let state = self.state.read().await;
            if !state.projects.contains_key(&project_id) {
                return Err(ServerError::ProjectNotFound(project_id));
            }
        }

        // Write-through to store
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_automation(&automation) {
                    tracing::error!(automation_id = %automation_id, error = %e, "failed to persist automation to store");
                }
            }
        }

        {
            let mut state = self.state.write().await;
            state.automations.insert(automation_id.clone(), automation);
        }

        // Emit automation created event
        let event = Event::new(
            EventType::AutomationCreated,
            &automation_id,
            Actor::Human,
            serde_json::json!({ "project_id": project_id }),
        );
        self.event_bus.publish(event).await?;

        Ok(())
    }

    /// Get an automation by ID.
    pub async fn get_automation(&self, id: &str) -> Option<Automation> {
        let state = self.state.read().await;
        state.automations.get(id).cloned()
    }

    /// Update an automation.
    pub async fn update_automation(
        &self,
        id: &str,
        name: Option<String>,
        prompt: Option<String>,
        state_update: Option<AutomationState>,
        trigger: Option<TriggerType>,
    ) -> Result<Automation, ServerError> {
        let mut automation = {
            let state = self.state.read().await;
            state
                .automations
                .get(id)
                .cloned()
                .ok_or_else(|| ServerError::StoreError(format!("automation not found: {}", id)))?
        };

        // Apply updates
        if let Some(name) = name {
            automation.name = name;
        }
        if let Some(prompt) = prompt {
            automation.prompt = prompt;
        }
        if let Some(new_state) = state_update {
            automation.state = new_state;
        }
        if let Some(trigger) = trigger {
            automation.trigger = trigger;
        }
        automation.updated_at = chrono::Utc::now();

        // Write-through to store
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_automation(&automation) {
                    tracing::error!(automation_id = %id, error = %e, "failed to persist automation update to store");
                }
            }
        }

        {
            let mut state = self.state.write().await;
            state.automations.insert(id.to_string(), automation.clone());
        }

        // Emit automation updated event
        let event = Event::new(
            EventType::AutomationUpdated,
            id,
            Actor::Human,
            serde_json::json!({}),
        );
        self.event_bus.publish(event).await?;

        Ok(automation)
    }

    /// Remove an automation.
    pub async fn remove_automation(&self, id: &str) -> bool {
        let removed = {
            let mut state = self.state.write().await;
            state.automations.remove(id).is_some()
        };

        if removed {
            // Delete from store
            if let Some(ref store) = self.store {
                if let Ok(store) = store.lock() {
                    if let Err(e) = store.delete_automation(id) {
                        tracing::error!(automation_id = %id, error = %e, "failed to delete automation from store");
                    }
                }
            }

            // Emit automation deleted event
            let event = Event::new(
                EventType::AutomationDeleted,
                id,
                Actor::Human,
                serde_json::json!({}),
            );
            if let Err(e) = self.event_bus.publish(event).await {
                tracing::error!(automation_id = %id, error = %e, "failed to publish automation deleted event");
            }

            tracing::info!(automation_id = %id, "automation removed");
        }

        removed
    }

    /// List runs for an automation.
    pub fn list_automation_runs(&self, automation_id: &str) -> Result<Vec<AutomationRun>, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.list_automation_runs(automation_id)
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    /// Create a new automation run.
    pub async fn create_automation_run(&self, automation_id: &str) -> Result<AutomationRun, ServerError> {
        // Verify automation exists
        {
            let state = self.state.read().await;
            if !state.automations.contains_key(automation_id) {
                return Err(ServerError::StoreError(format!("automation not found: {}", automation_id)));
            }
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let mut run = AutomationRun::new(&run_id, automation_id);
        run.start();

        // Save to store
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_automation_run(&run) {
                    tracing::error!(run_id = %run_id, error = %e, "failed to persist automation run to store");
                }
            }
        }

        // Emit run started event
        let event = Event::new(
            EventType::AutomationRunStarted,
            &run_id,
            Actor::System,
            serde_json::json!({ "automation_id": automation_id }),
        );
        self.event_bus.publish(event).await?;

        Ok(run)
    }

    /// Complete an automation run with output.
    pub async fn complete_automation_run(
        &self,
        run_id: &str,
        output: Option<String>,
    ) -> Result<AutomationRun, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;

        let mut run = {
            let store = store.lock().unwrap();
            store.get_automation_run(run_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?
                .ok_or_else(|| ServerError::StoreError(format!("run not found: {}", run_id)))?
        };

        run.complete(output.clone());

        // Save to store
        {
            let store = store.lock().unwrap();
            store.save_automation_run(&run)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
        }

        // Emit run completed event
        let event = Event::new(
            EventType::AutomationRunCompleted,
            run_id,
            Actor::System,
            serde_json::json!({
                "automation_id": run.automation_id,
                "output": output,
            }),
        );
        self.event_bus.publish(event).await?;

        tracing::info!(run_id = %run_id, "Automation run completed");
        Ok(run)
    }

    /// Fail an automation run with an error message.
    pub async fn fail_automation_run(
        &self,
        run_id: &str,
        error: impl Into<String>,
    ) -> Result<AutomationRun, ServerError> {
        let error_str = error.into();
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;

        let mut run = {
            let store = store.lock().unwrap();
            store.get_automation_run(run_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?
                .ok_or_else(|| ServerError::StoreError(format!("run not found: {}", run_id)))?
        };

        run.fail(&error_str);

        // Save to store
        {
            let store = store.lock().unwrap();
            store.save_automation_run(&run)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
        }

        // Emit run failed event
        let event = Event::new(
            EventType::AutomationRunFailed,
            run_id,
            Actor::System,
            serde_json::json!({
                "automation_id": run.automation_id,
                "error": error_str,
            }),
        );
        self.event_bus.publish(event).await?;

        tracing::warn!(run_id = %run_id, error = %error_str, "Automation run failed");
        Ok(run)
    }

    /// Cancel an automation run.
    pub async fn cancel_automation_run(
        &self,
        run_id: &str,
    ) -> Result<AutomationRun, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;

        let mut run = {
            let store = store.lock().unwrap();
            store.get_automation_run(run_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?
                .ok_or_else(|| ServerError::RunNotFound(run_id.to_string()))?
        };

        // Only allow cancelling runs that are pending or running
        if run.status.is_terminal() {
            return Err(ServerError::InvalidOperation(format!(
                "cannot cancel run in terminal state: {:?}",
                run.status
            )));
        }

        run.cancel();

        // Save to store
        {
            let store = store.lock().unwrap();
            store.save_automation_run(&run)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
        }

        // Emit run cancelled event
        let event = Event::new(
            EventType::AutomationRunCancelled,
            run_id,
            Actor::Human,
            serde_json::json!({
                "automation_id": run.automation_id,
            }),
        );
        self.event_bus.publish(event).await?;

        tracing::info!(run_id = %run_id, "Automation run cancelled");
        Ok(run)
    }

    /// Get the automation associated with a run.
    pub async fn get_automation_for_run(&self, run_id: &str) -> Result<Option<Automation>, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;

        let run = {
            let store = store.lock().unwrap();
            store.get_automation_run(run_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?
        };

        match run {
            Some(run) => {
                let state = self.state.read().await;
                Ok(state.automations.get(&run.automation_id).cloned())
            }
            None => Ok(None),
        }
    }

    // --- Mode management (spec Section 6) ---

    pub async fn mode(&self) -> Mode {
        self.state.read().await.mode
    }

    /// Transition the operating mode.
    ///
    /// Enforces spec Section 6.4 rules:
    /// - Human can change in any direction
    /// - Orchestrator can only lower
    /// - Takes effect immediately
    pub async fn set_mode(
        &self,
        target: Mode,
        actor: &Actor,
    ) -> Result<Mode, ServerError> {
        let mut state = self.state.write().await;
        let new_mode = state.mode.transition(target, actor)?;
        let old_mode = state.mode;
        state.mode = new_mode;

        // Emit the appropriate mode event
        let event_type = match new_mode {
            Mode::Stop => EventType::SystemModeStop,
            Mode::Pause => EventType::SystemModePause,
            Mode::Play => EventType::SystemModePlay,
        };

        drop(state);

        // Use "system" as the task ID for system events
        let event = Event::new(
            event_type,
            "system",
            actor.clone(),
            serde_json::json!({ "from": format!("{:?}", old_mode) }),
        );
        self.event_bus.publish(event).await?;

        Ok(new_mode)
    }

    // --- Task management (spec Section 5) ---

    pub async fn add_task(&self, task: Task) -> Result<(), ServerError> {
        let task_id = task.id.clone();
        let project_id = task.project.clone();

        {
            let state = self.state.read().await;
            if !state.projects.contains_key(&project_id) {
                return Err(ServerError::ProjectNotFound(project_id));
            }
        }

        let event = Event::new(
            EventType::TaskCreated,
            &task_id,
            Actor::System,
            serde_json::json!({
                "title": task.title,
                "project": task.project,
            }),
        );

        // Write-through to store before inserting into HashMap
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_task(&task) {
                    tracing::error!(task_id = %task_id, error = %e, "failed to persist task to store");
                }
            }
        }

        {
            let mut state = self.state.write().await;
            state.tasks.insert(task_id, task);
        }

        self.event_bus.publish(event).await?;
        Ok(())
    }

    pub async fn get_task(&self, id: &str) -> Option<Task> {
        let state = self.state.read().await;
        state.tasks.get(id).cloned()
    }

    /// Transition a task's state and emit the corresponding event.
    /// Update task state, persist to store, and publish an event.
    ///
    /// If the task is already in a terminal state, this is a no-op to prevent
    /// duplicate events when multiple paths detect the same completion
    /// (e.g., issue closure + PR merge in the same poll cycle).
    pub async fn set_task_state(
        &self,
        task_id: &str,
        new_state: TaskState,
        actor: Actor,
    ) -> Result<(), ServerError> {
        self.set_task_state_with_data(task_id, new_state, actor, serde_json::json!({}))
            .await
    }

    /// Transition a task's state and emit the corresponding event with custom data.
    ///
    /// Like [`set_task_state`], but allows passing custom event data for
    /// downstream consumers. Use this when the state change source needs to
    /// be distinguished (e.g., `{ "source": "reconciliation" }` for external
    /// closure detection during polling).
    pub async fn set_task_state_with_data(
        &self,
        task_id: &str,
        new_state: TaskState,
        actor: Actor,
        data: serde_json::Value,
    ) -> Result<(), ServerError> {
        // Check if task is already terminal to prevent duplicate events
        let current_state = {
            let state = self.state.read().await;
            state
                .tasks
                .get(task_id)
                .map(|t| t.state)
                .ok_or_else(|| ServerError::TaskNotFound(task_id.to_string()))?
        };

        if current_state.is_terminal() {
            tracing::debug!(
                task_id = %task_id,
                current_state = ?current_state,
                requested_state = ?new_state,
                "skipping state transition: task already in terminal state"
            );
            return Ok(());
        }

        let applied = self.apply_task_state(task_id, new_state).await?;
        if !applied {
            // The authoritative guard inside apply_task_state (under the
            // write lock) determined the task is already terminal — skip
            // the event to avoid duplicates.
            return Ok(());
        }

        let event_type = match new_state {
            TaskState::Waiting => EventType::TaskStateWaiting,
            TaskState::Blocked => EventType::TaskStateBlocked,
            TaskState::Running => EventType::TaskStateRunning,
            TaskState::Question => EventType::TaskStateQuestion,
            TaskState::Testing => EventType::TaskStateTesting,
            TaskState::AwaitingMerge => EventType::TaskStateAwaitingMerge,
            TaskState::Conflict => EventType::TaskStateConflict,
            TaskState::ChangesRequested => EventType::TaskStateChangesRequested,
            TaskState::Completed => EventType::TaskStateCompleted,
            TaskState::Failed => EventType::TaskStateFailed,
            TaskState::Cancelled => EventType::TaskStateCancelled,
        };

        let event = Event::new(event_type, task_id, actor, data);
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Update task state and persist to store, **without publishing an event**.
    ///
    /// Only call this from the event-handler loop in `app`, where the
    /// state-change event was already published by the session monitor.
    /// All other callers should use [`set_task_state`], which publishes
    /// the corresponding event.
    ///
    /// Returns `true` if the state was applied, `false` if the task was
    /// already in a terminal state.  This is the authoritative guard
    /// against TOCTOU races — it checks under the write lock so two
    /// concurrent callers cannot both pass the terminal-state check.
    pub async fn apply_task_state(
        &self,
        task_id: &str,
        new_state: TaskState,
    ) -> Result<bool, ServerError> {
        let mut state = self.state.write().await;
        let task = state
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| ServerError::TaskNotFound(task_id.to_string()))?;

        // Authoritative terminal-state guard under the write lock.
        // The read-lock pre-check in set_task_state is just a fast path;
        // this is the real guard that prevents duplicate transitions.
        if task.state.is_terminal() {
            tracing::debug!(
                task_id = %task_id,
                current_state = ?task.state,
                requested_state = ?new_state,
                "apply_task_state: task already terminal, skipping"
            );
            return Ok(false);
        }

        task.set_state(new_state);

        // Write-through to store
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_task(task) {
                    tracing::error!(task_id = %task_id, error = %e, "failed to persist task state to store");
                }
            }
        }
        Ok(true)
    }

    /// Update a task's priority and persist to store.
    ///
    /// Used for manual queue reordering via the GUI. Lower numbers are
    /// higher priority (dispatched first).
    pub async fn set_task_priority(
        &self,
        task_id: &str,
        priority: Option<i32>,
        actor: Actor,
    ) -> Result<(), ServerError> {
        {
            let mut state = self.state.write().await;
            let task = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| ServerError::TaskNotFound(task_id.to_string()))?;

            task.priority = priority;
            task.updated_at = chrono::Utc::now();

            // Write-through to store
            if let Some(ref store) = self.store {
                if let Ok(store) = store.lock() {
                    if let Err(e) = store.save_task(task) {
                        tracing::error!(task_id = %task_id, error = %e, "failed to persist task priority to store");
                    }
                }
            }
        }

        // Emit task:updated event to notify clients
        let event = Event::new(
            EventType::TaskUpdated,
            task_id,
            actor,
            serde_json::json!({ "priority": priority }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Reorder tasks by updating their priorities.
    ///
    /// Takes a list of task IDs in the desired order and assigns sequential
    /// priorities (1, 2, 3, ...) to them. Tasks not in the list keep their
    /// current priority.
    pub async fn reorder_tasks(
        &self,
        task_ids: &[String],
        actor: Actor,
    ) -> Result<(), ServerError> {
        {
            let mut state = self.state.write().await;
            let now = chrono::Utc::now();

            for (index, task_id) in task_ids.iter().enumerate() {
                if let Some(task) = state.tasks.get_mut(task_id) {
                    task.priority = Some((index + 1) as i32);
                    task.updated_at = now;

                    // Write-through to store
                    if let Some(ref store) = self.store {
                        if let Ok(store) = store.lock() {
                            if let Err(e) = store.save_task(task) {
                                tracing::error!(task_id = %task_id, error = %e, "failed to persist task priority to store");
                            }
                        }
                    }
                }
            }
        }

        // Emit task:reordered event to notify clients
        let event = Event::new(
            EventType::TaskReordered,
            "",
            actor,
            serde_json::json!({ "task_ids": task_ids }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    // --- Merge queue (spec §7) ---

    /// Add an entry to the merge queue, persisting to store and publishing
    /// a `merge:queued` event (spec §7.1 step 3).
    pub async fn add_to_merge_queue(
        &self,
        entry: crate::model::merge_queue::MergeQueueEntry,
    ) -> Result<(), ServerError> {
        let task_id = entry.task_id.clone();
        let entry_id = entry.id.clone();
        let pr_url = entry.pr_url.clone();
        {
            let mut state = self.state.write().await;
            // Dedup by PR URL first (handles unlinked PRs where task_id is empty)
            if state.merge_queue.get_by_pr_url(&pr_url).is_some() {
                return Ok(());
            }
            // Dedup by task_id for linked PRs
            if !task_id.is_empty() && state.merge_queue.get_by_task(&task_id).is_some() {
                return Ok(());
            }
            if let Some(ref store) = self.store {
                if let Ok(store) = store.lock() {
                    if let Err(e) = store.save_merge_entry(&entry) {
                        tracing::error!(entry_id = %entry_id, error = %e, "failed to persist merge queue entry");
                    }
                }
            }
            state.merge_queue.enqueue(entry);
        }

        let event = Event::new(
            EventType::MergeQueued,
            &task_id,
            Actor::System,
            serde_json::json!({ "entry_id": entry_id }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    // --- Dispatch integration (spec §12.6) ---

    /// Check if a task with the given source already exists (dedup for scheduler).
    pub async fn has_task_for_source(&self, source: &TaskSource) -> bool {
        let state = self.state.read().await;
        crate::scheduler::task_exists_for_source(&state.tasks, source)
    }

    /// Find the task ID for a given source, if one exists.
    pub async fn task_id_for_source(&self, source: &TaskSource) -> Option<String> {
        let state = self.state.read().await;
        state.tasks.values()
            .find(|t| t.source == *source)
            .map(|t| t.id.clone())
    }

    /// Check if a PR URL is already in the merge queue.
    pub async fn has_merge_entry_for_pr(&self, pr_url: &str) -> bool {
        let state = self.state.read().await;
        state.merge_queue.get_by_pr_url(pr_url).is_some()
    }

    /// Find a task ID by its branch name.
    ///
    /// Tasks use branches named `tasks/{task_id}--{unique_suffix}` (new format)
    /// or `tasks/{task_id}` (legacy format). The `--` delimiter separates the
    /// task ID from the unique suffix added to prevent branch name clashes.
    pub async fn find_task_by_branch(&self, branch: &str) -> Option<String> {
        // Strip the "tasks/" prefix
        let branch_suffix = branch.strip_prefix("tasks/")?;
        let state = self.state.read().await;

        // New format: "tasks/{task_id}--{unique_suffix}"
        // Split on "--" and check if the first part is a known task ID.
        if let Some((task_id, _suffix)) = branch_suffix.split_once("--") {
            if state.tasks.contains_key(task_id) {
                return Some(task_id.to_string());
            }
        }

        // Legacy format: "tasks/{task_id}" (exact match)
        if state.tasks.contains_key(branch_suffix) {
            return Some(branch_suffix.to_string());
        }

        None
    }

    /// Find a task ID by its linked GitHub issue.
    ///
    /// This is a fallback lookup for when branch name matching fails. It searches
    /// for tasks whose source is the specified GitHub issue coordinates.
    pub async fn find_task_by_github_issue(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Option<String> {
        let state = self.state.read().await;
        state.tasks.values()
            .find(|task| {
                matches!(
                    &task.source,
                    TaskSource::GithubIssue { owner: o, repo: r, number: n }
                    if o == owner && r == repo && *n == issue_number
                )
            })
            .map(|task| task.id.clone())
    }

    // --- Reconciliation (issue #254, #255) ---

    /// Reconcile a task with fresh GitHub issue data.
    ///
    /// Updates GitHub-authoritative fields and persists to store.
    /// Returns the reconciliation result for logging/events.
    pub async fn reconcile_task(
        &self,
        task_id: &str,
        issue: &tasks_github::model::Issue,
        label_config: &crate::workflow::LabelConfig,
    ) -> Result<Option<crate::scheduler::ReconcileResult>, ServerError> {
        let result = {
            let mut state = self.state.write().await;
            let task = match state.tasks.get_mut(task_id) {
                Some(t) => t,
                None => return Ok(None),
            };

            let result = crate::scheduler::reconcile_task(task, issue, label_config);

            if result.has_changes() {
                // Write-through to store
                if let Some(ref store) = self.store {
                    if let Ok(store) = store.lock() {
                        if let Err(e) = store.save_task(task) {
                            tracing::error!(task_id = %task_id, error = %e, "failed to persist reconciled task");
                        }
                    }
                }
            }

            result
        };

        // Emit state change events outside the write lock
        if let Some(new_state) = result.new_state {
            let event_type = match new_state {
                TaskState::Waiting => EventType::TaskStateWaiting,
                TaskState::Blocked => EventType::TaskStateBlocked,
                TaskState::Completed => EventType::TaskStateCompleted,
                TaskState::Cancelled => EventType::TaskStateCancelled,
                _ => EventType::TaskStateWaiting, // shouldn't happen from reconciliation
            };

            let event = Event::new(
                event_type,
                task_id,
                Actor::Scheduler,
                serde_json::json!({ "source": "reconciliation" }),
            );
            self.event_bus.publish(event).await?;
        }

        Ok(Some(result))
    }

    /// Reconcile merge queue entries against fresh PR data from GitHub.
    ///
    /// For each PR in the poll results:
    /// - If PR is merged and has a merge entry → mark entry as Merged
    /// - If PR is closed (not merged) and has a merge entry → remove entry
    /// - If PR is open and has a merge entry → update conflict status
    ///
    /// Returns the number of entries updated/removed.
    ///
    /// ## Performance
    ///
    /// This method builds a HashMap index of merge queue entries in a single
    /// read pass, then computes all actions with O(1) lookups per PR. This
    /// avoids O(N*M) lock churn from acquiring a lock and doing a linear scan
    /// for each of N PRs across M merge queue entries.
    pub async fn reconcile_merge_queue(
        &self,
        prs: &[tasks_github::model::PullRequest],
    ) -> Result<u32, ServerError> {
        // Build a HashMap index of pr_url -> (entry_id, task_id, status) in one read pass.
        // This avoids O(N*M) from acquiring a lock and linear scanning for each PR.
        let entry_index: HashMap<String, (String, String, MergeStatus)> = {
            let state = self.state.read().await;
            state
                .merge_queue
                .entries()
                .iter()
                .map(|e| (e.pr_url.clone(), (e.id.clone(), e.task_id.clone(), e.status)))
                .collect()
        };

        // Compute all actions with O(1) lookups
        let mut actions = Vec::new();
        for pr in prs {
            let pr_url = format!(
                "https://github.com/{}/{}/pull/{}",
                pr.owner, pr.repo, pr.number
            );

            let Some((entry_id, task_id, current_status)) = entry_index.get(&pr_url) else {
                continue;
            };
            let entry_id = entry_id.clone();
            let task_id = task_id.clone();

            let (action, entry_id_for_sha_update) = match pr.state {
                tasks_github::model::PullRequestState::Merged => {
                    if *current_status != MergeStatus::Merged {
                        (Some(MqAction::MarkMerged { entry_id, task_id, pr_url }), None)
                    } else {
                        continue;
                    }
                }
                tasks_github::model::PullRequestState::Closed => {
                    (Some(MqAction::Remove { entry_id, pr_url }), None)
                }
                tasks_github::model::PullRequestState::Open => {
                    // For open PRs, always update head_sha to detect new commits
                    let sha_update_id = entry_id.clone();
                    // Update conflict status from GitHub's mergeable field
                    let conflict_action = match pr.mergeable {
                        Some(tasks_github::model::MergeableState::Conflicting)
                            if *current_status != MergeStatus::Conflict =>
                        {
                            Some(MqAction::MarkConflict { entry_id, pr_url })
                        }
                        Some(tasks_github::model::MergeableState::Mergeable)
                            if *current_status == MergeStatus::Conflict =>
                        {
                            Some(MqAction::ClearConflict { entry_id })
                        }
                        _ => None,
                    };
                    (conflict_action, Some(sha_update_id))
                }
            };
            actions.push((action, entry_id_for_sha_update, pr.head_sha.clone()));
        }

        // Execute all actions
        let mut changes = 0u32;
        for (action, entry_id_for_sha_update, head_sha) in actions {
            if let Some(action) = action {
                match action {
                    MqAction::MarkMerged { entry_id, pr_url, .. } => {
                        tracing::info!(
                            entry_id = %entry_id,
                            pr_url = %pr_url,
                            "reconciliation: PR merged externally, marking entry as merged"
                        );
                        // mark_entry_merged also transitions the linked task to Completed
                        if let Err(e) = self.mark_entry_merged(&entry_id, &pr_url).await {
                            tracing::warn!(entry_id = %entry_id, error = %e, "failed to mark entry as merged during reconciliation");
                        } else {
                            changes += 1;
                        }
                    }
                    MqAction::Remove { entry_id, pr_url } => {
                        tracing::info!(
                            entry_id = %entry_id,
                            pr_url = %pr_url,
                            "reconciliation: PR closed without merge, removing entry"
                        );
                        let mut state = self.state.write().await;
                        state.merge_queue.remove_by_pr_url(&pr_url);
                        // Also remove from store
                        if let Some(ref store) = self.store {
                            if let Ok(store) = store.lock() {
                                if let Err(e) = store.delete_merge_entry(&entry_id) {
                                    tracing::error!(entry_id = %entry_id, error = %e, "failed to delete merge entry from store");
                                }
                            }
                        }
                        changes += 1;
                    }
                    MqAction::MarkConflict { entry_id, pr_url } => {
                        tracing::debug!(
                            entry_id = %entry_id,
                            "reconciliation: PR has merge conflict"
                        );
                        let conflict_info = crate::model::merge_queue::ConflictInfo::new(
                            crate::model::merge_queue::ConflictType::Unknown,
                            "Conflict detected from GitHub mergeable status",
                        );
                        if let Err(e) = self.mark_entry_conflict(&entry_id, &pr_url, Some(conflict_info)).await {
                            tracing::warn!(entry_id = %entry_id, error = %e, "failed to mark conflict during reconciliation");
                        } else {
                            changes += 1;
                        }
                    }
                    MqAction::ClearConflict { entry_id } => {
                        tracing::debug!(
                            entry_id = %entry_id,
                            "reconciliation: conflict resolved"
                        );
                        if let Err(e) = self.clear_entry_conflict(&entry_id).await {
                            tracing::warn!(entry_id = %entry_id, error = %e, "failed to clear conflict during reconciliation");
                        } else {
                            changes += 1;
                        }
                    }
                }
            }

            // Update head_sha for open PRs to detect new commits
            if let Some(entry_id) = entry_id_for_sha_update {
                let mut state = self.state.write().await;
                match state.merge_queue.update_head_sha(&entry_id, &head_sha) {
                    Ok(true) => {
                        tracing::debug!(
                            entry_id = %entry_id,
                            head_sha = %head_sha,
                            "reconciliation: updated head_sha for PR"
                        );
                    }
                    Ok(false) => {} // No change
                    Err(e) => {
                        tracing::warn!(entry_id = %entry_id, error = %e, "failed to update head_sha during reconciliation");
                    }
                }
            }
        }

        Ok(changes)
    }

    /// Get the effective session limit for a project.
    /// Returns the project's configured limit, or the global default.
    pub async fn project_session_limit(&self, project_id: &str, global_max: u32) -> u32 {
        let state = self.state.read().await;
        state
            .projects
            .get(project_id)
            .and_then(|p| {
                // Try to parse workflow config from project config
                // For now, return None — workflow config integration is future work
                let _ = p;
                None::<u32>
            })
            .unwrap_or(global_max)
    }

    /// Run a dispatch evaluation and process the results (spec §12.6).
    ///
    /// This is called by event handlers and the reconciliation tick.
    /// For now, it selects candidates and transitions state. Actual session
    /// creation (containers + agents) will be wired up when the full pipeline
    /// is integrated.
    pub async fn run_dispatch(
        &self,
        pending_answers: &[String],
        global_max: u32,
    ) -> Result<DispatchPlan, ServerError> {
        self.run_dispatch_with_limits(pending_answers, global_max, None).await
    }

    pub async fn run_dispatch_with_limits(
        &self,
        pending_answers: &[String],
        global_max: u32,
        per_project_limits: Option<&HashMap<String, u32>>,
    ) -> Result<DispatchPlan, ServerError> {
        // 1. Read current mode — if Stop, return empty plan.
        {
            let state = self.state.read().await;
            if !state.mode.allows_dispatch() {
                return Ok(DispatchPlan {
                    resume: Vec::new(),
                    new_work: Vec::new(),
                });
            }
        }

        // 2. Unblock tasks: find tasks in Blocked state where all blocked_by
        //    tasks are in terminal state, transition them to Waiting.
        {
            let state = self.state.read().await;
            let to_unblock: Vec<String> = state
                .tasks
                .values()
                .filter(|t| t.state == TaskState::Blocked)
                .filter(|t| {
                    t.blocked_by.iter().all(|dep_id| {
                        state
                            .tasks
                            .get(dep_id)
                            .is_some_and(|dep| dep.state.is_terminal())
                    })
                })
                .map(|t| t.id.clone())
                .collect();
            drop(state);

            for task_id in to_unblock {
                self.set_task_state(&task_id, TaskState::Waiting, Actor::System)
                    .await?;
            }
        }

        // 3. Build project_limits map from per-project overrides or global default.
        let project_limits = {
            let state = self.state.read().await;
            let mut limits = HashMap::new();
            for project_id in state.projects.keys() {
                let limit = per_project_limits
                    .and_then(|m| m.get(project_id).copied())
                    .unwrap_or(global_max);
                limits.insert(project_id.clone(), limit);
            }
            limits
        };

        // 4. Call dispatcher::evaluate() with current tasks.
        //    Build set of task IDs with unclosed PRs in merge queue (skip these).
        let plan = {
            let state = self.state.read().await;
            let tasks_with_active_prs: HashSet<String> = state
                .merge_queue
                .entries()
                .iter()
                .filter(|e| matches!(
                    e.status,
                    MergeStatus::Pending | MergeStatus::Approved | MergeStatus::Conflict
                ))
                .map(|e| e.task_id.clone())
                .collect();
            dispatcher::evaluate(
                &state.tasks,
                pending_answers,
                &tasks_with_active_prs,
                &project_limits,
                global_max,
            )
        };

        // 5. For each task in plan.new_work: transition to Running.
        for task_id in &plan.new_work {
            self.set_task_state(task_id, TaskState::Running, Actor::System)
                .await?;
        }

        // 6. For each task in plan.resume: transition to Running.
        for task_id in &plan.resume {
            self.set_task_state(task_id, TaskState::Running, Actor::System)
                .await?;
        }

        // 7. Return the plan.
        Ok(plan)
    }

    // --- Merge queue (spec Section 7) ---

    /// Collect approved entries for flush (spec Section 6.2).
    ///
    /// Only valid in Pause mode. Returns (entry_id, pr_url) pairs for
    /// the caller to execute the actual GitHub merges. Does NOT mark entries
    /// as merged - the caller must call mark_entry_merged() or mark_entry_conflict()
    /// based on the result of the GitHub API call.
    pub async fn collect_entries_for_flush(&self) -> Result<Vec<(String, String)>, ServerError> {
        let state = self.state.read().await;
        let mode = state.mode;
        state
            .merge_queue
            .collect_approved_for_flush(mode)
            .map_err(|e| ServerError::EventStore(events::StoreError::Io(
                std::io::Error::other(e.to_string()),
            )))
    }

    /// Emit the system:flush event after merges have been processed.
    ///
    /// This should be called after all merge operations are complete.
    pub async fn emit_flush_event(&self, merged_ids: &[String]) -> Result<(), ServerError> {
        let event = Event::new(
            EventType::SystemFlush,
            "system",
            Actor::Human,
            serde_json::json!({ "flushed": merged_ids }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Approve a merge queue entry and emit a `merge:approved` event.
    ///
    /// In Play mode, this is called by the orchestrator loop after a
    /// positive evaluation. The entry transitions from Pending to Approved.
    pub async fn approve_merge_entry(
        &self,
        entry_id: &str,
        reasoning: &str,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .approve(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        let event = Event::new(
            EventType::MergeApproved,
            &task_id,
            Actor::Orchestrator,
            serde_json::json!({ "entry_id": entry_id, "reasoning": reasoning }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Mark a merge queue entry as actively merging (GitHub API call in progress).
    /// Emits `merge:merging` event.
    pub async fn mark_entry_merging(
        &self,
        entry_id: &str,
        pr_url: &str,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .mark_merging(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        let event = Event::new(
            EventType::MergeMerging,
            &task_id,
            Actor::System,
            serde_json::json!({ "entry_id": entry_id, "pr_url": pr_url }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Mark a merge queue entry as merged and transition the linked task
    /// to Completed. Emits `merge:completed` and `task:state:completed`.
    pub async fn mark_entry_merged(
        &self,
        entry_id: &str,
        pr_url: &str,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .mark_merged(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        let event = Event::new(
            EventType::MergeCompleted,
            &task_id,
            Actor::System,
            serde_json::json!({ "entry_id": entry_id, "pr_url": pr_url }),
        );
        self.event_bus.publish(event).await?;

        // Transition linked task to Completed (if task_id is non-empty)
        if !task_id.is_empty() {
            self.set_task_state(&task_id, TaskState::Completed, Actor::System)
                .await?;
        }

        Ok(())
    }

    /// Mark a merge queue entry as conflicted. Emits `merge:conflict`.
    ///
    /// Optionally accepts `ConflictInfo` with details about the conflict type
    /// and affected files (spec §7.4).
    pub async fn mark_entry_conflict(
        &self,
        entry_id: &str,
        pr_url: &str,
        conflict_info: Option<crate::model::merge_queue::ConflictInfo>,
    ) -> Result<(), ServerError> {
        let (task_id, conflict_type) = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .mark_conflict(entry_id, conflict_info.clone())
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            let conflict_type = entry
                .conflict_info
                .as_ref()
                .map(|i| format!("{:?}", i.conflict_type));
            (entry.task_id.clone(), conflict_type)
        };

        let mut event_data = serde_json::json!({ "entry_id": entry_id, "pr_url": pr_url });
        if let Some(ct) = conflict_type {
            event_data["conflict_type"] = serde_json::Value::String(ct);
        }
        if let Some(ref info) = conflict_info {
            event_data["conflicting_files"] =
                serde_json::Value::Array(info.conflicting_files.iter().map(|f| serde_json::Value::String(f.clone())).collect());
        }

        let event = Event::new(EventType::MergeConflict, &task_id, Actor::System, event_data);
        self.event_bus.publish(event).await?;

        // Also transition the task to Conflict state
        if !task_id.is_empty() {
            self.set_task_state(&task_id, TaskState::Conflict, Actor::System)
                .await?;
        }

        Ok(())
    }

    /// Re-engage a task to resolve a conflict (spec §7.4).
    ///
    /// Transitions the task back to Waiting with conflict feedback so the
    /// dispatch loop picks it up. The conflict info is preserved in the
    /// merge queue entry.
    pub async fn reengage_for_conflict(
        &self,
        entry_id: &str,
        feedback: &str,
    ) -> Result<(), ServerError> {
        let task_id = {
            let state = self.state.read().await;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        // Emit orchestrator feedback event
        self.emit_orchestrator_feedback(&task_id, feedback, Some("conflict_resolution"))
            .await?;

        // Transition task back to Waiting for re-dispatch
        self.set_task_state(&task_id, TaskState::Waiting, Actor::Orchestrator)
            .await?;

        Ok(())
    }

    /// Clear conflict status from a merge queue entry after resolution.
    ///
    /// This transitions the entry back to Pending and the task back to
    /// AwaitingMerge.
    pub async fn clear_entry_conflict(&self, entry_id: &str) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .clear_conflict(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        // Transition task back to AwaitingMerge
        if !task_id.is_empty() {
            self.set_task_state(&task_id, TaskState::AwaitingMerge, Actor::System)
                .await?;
        }

        Ok(())
    }

    /// Reject a merge queue entry, re-dispatch the task with feedback,
    /// and emit a `merge:rejected` event.
    ///
    /// The task transitions back to Waiting so the dispatch loop picks
    /// it up and starts a fresh session with the feedback as context.
    pub async fn reject_merge_entry(
        &self,
        entry_id: &str,
        reasoning: &str,
        feedback: Option<&str>,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .reject(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        // Emit rejection event
        let event = Event::new(
            EventType::MergeRejected,
            &task_id,
            Actor::Orchestrator,
            serde_json::json!({
                "entry_id": entry_id,
                "reasoning": reasoning,
                "feedback": feedback,
            }),
        );
        self.event_bus.publish(event).await?;

        // Transition task back to Waiting for re-dispatch (scenario 2)
        self.set_task_state(&task_id, TaskState::Waiting, Actor::Orchestrator)
            .await?;

        Ok(())
    }

    /// Reject a merge queue entry when the underlying issue is already closed.
    ///
    /// Unlike `reject_merge_entry`, this does NOT transition the task back to
    /// Waiting. Instead, it marks the task as Completed (or Cancelled if
    /// NOT_PLANNED) since there's no reason to re-dispatch.
    ///
    /// This prevents the infinite loop described in issue #132 where rejected
    /// tasks for closed issues would cycle endlessly.
    pub async fn reject_merge_entry_closed(
        &self,
        entry_id: &str,
        reasoning: &str,
        final_state: TaskState,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .reject(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        // Emit rejection event with context that the issue is closed
        let event = Event::new(
            EventType::MergeRejected,
            &task_id,
            Actor::Orchestrator,
            serde_json::json!({
                "entry_id": entry_id,
                "reasoning": reasoning,
                "issue_closed": true,
            }),
        );
        self.event_bus.publish(event).await?;

        // Transition task to terminal state (Completed or Cancelled)
        self.set_task_state(&task_id, final_state, Actor::Orchestrator)
            .await?;

        Ok(())
    }

    /// Request changes on a merge queue entry.
    ///
    /// Unlike rejection, the entry stays in the queue with status `ChangesRequested`.
    /// The task transitions to `ChangesRequested` state, which gets priority dispatch
    /// over regular `Waiting` tasks.
    ///
    /// This preserves the PR and work-in-progress rather than throwing it away,
    /// allowing the agent to address feedback and re-submit.
    pub async fn request_changes_merge_entry(
        &self,
        entry_id: &str,
        reasoning: &str,
        feedback: &str,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .request_changes(entry_id, feedback)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        // Emit changes requested event
        let event = Event::new(
            EventType::MergeChangesRequested,
            &task_id,
            Actor::Orchestrator,
            serde_json::json!({
                "entry_id": entry_id,
                "reasoning": reasoning,
                "feedback": feedback,
            }),
        );
        self.event_bus.publish(event).await?;

        // Transition task to ChangesRequested state for priority dispatch
        self.set_task_state(&task_id, TaskState::ChangesRequested, Actor::Orchestrator)
            .await?;

        Ok(())
    }

    /// Clear changes requested status after agent addresses feedback.
    ///
    /// Returns the entry to Pending status for re-evaluation.
    pub async fn clear_changes_requested(
        &self,
        entry_id: &str,
    ) -> Result<(), ServerError> {
        let task_id = {
            let mut state = self.state.write().await;
            state
                .merge_queue
                .clear_changes_requested(entry_id)
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let entry = state.merge_queue.get(entry_id)
                .ok_or_else(|| ServerError::StoreError(format!("entry not found: {}", entry_id)))?;
            entry.task_id.clone()
        };

        // Transition task back to AwaitingMerge for re-evaluation
        self.set_task_state(&task_id, TaskState::AwaitingMerge, Actor::Orchestrator)
            .await?;

        Ok(())
    }

    /// Remove terminal entries (merged, rejected) from the merge queue.
    ///
    /// Should be called periodically to prevent unbounded queue growth.
    /// See issue #132.
    ///
    /// If `merged_cutoff` is provided, only removes Merged/Rejected entries that
    /// were completed before the cutoff. This implements a cooldown period to
    /// prevent race conditions with GitHub's API propagation. See issue #438.
    ///
    /// If `conflict_cutoff` is provided, also removes conflict entries that have
    /// been in conflict state since before the cutoff time. This prevents stale
    /// conflicts from accumulating indefinitely. See issue #282.
    pub async fn cleanup_merge_queue(
        &self,
        merged_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        conflict_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let mut state = self.state.write().await;
        state.merge_queue.cleanup(merged_cutoff, conflict_cutoff);
    }

    /// Emit an orchestrator:decision event recording an evaluation.
    ///
    /// Called for every evaluation regardless of mode — this is the
    /// audit trail of the orchestrator's judgment.
    pub async fn emit_orchestrator_decision(
        &self,
        task_id: &str,
        entry_id: &str,
        approved: bool,
        reasoning: &str,
    ) -> Result<(), ServerError> {
        let event = Event::new(
            EventType::OrchestratorDecision,
            task_id,
            Actor::Orchestrator,
            serde_json::json!({
                "entry_id": entry_id,
                "approved": approved,
                "reasoning": reasoning,
            }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Emit an orchestrator:feedback event when feedback is sent to a task.
    ///
    /// Called when the orchestrator sends guidance to a re-engaged task after
    /// rejection or when providing direction during a task's execution.
    /// Spec §8.3: "orchestrator:feedback — orchestrator sent feedback to an agent"
    pub async fn emit_orchestrator_feedback(
        &self,
        task_id: &str,
        feedback: &str,
        context: Option<&str>,
    ) -> Result<(), ServerError> {
        let event = Event::new(
            EventType::OrchestratorFeedback,
            task_id,
            Actor::Orchestrator,
            serde_json::json!({
                "feedback": feedback,
                "context": context,
            }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Emit an orchestrator:escalation event when surfacing issues to the human.
    ///
    /// Called when the orchestrator needs human attention for a decision,
    /// blocker, or important information that requires human review.
    /// Spec §8.3: "orchestrator:escalation — orchestrator surfaced something to the human"
    pub async fn emit_orchestrator_escalation(
        &self,
        task_id: &str,
        reason: &str,
        details: serde_json::Value,
    ) -> Result<(), ServerError> {
        let event = Event::new(
            EventType::OrchestratorEscalation,
            task_id,
            Actor::Orchestrator,
            serde_json::json!({
                "reason": reason,
                "details": details,
            }),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    // --- Presence (spec Section 4.1) ---

    /// Whether the human is present (has active GUI connections).
    pub fn is_human_present(&self) -> bool {
        self.presence.is_present()
    }

    // --- Workspace cleanup (spec §10.3) ---

    /// Get all workspace cleanup candidates.
    ///
    /// Returns tasks whose workspaces are eligible for cleanup based on:
    /// - Terminal state (Completed, Failed, Cancelled)
    /// - Stale/idle workspaces (no activity beyond threshold)
    ///
    /// PR merge cleanup is handled separately in the orchestrator loop.
    pub async fn get_workspace_cleanup_candidates(
        &self,
        stale_threshold: std::time::Duration,
    ) -> Vec<crate::workspace::CleanupCandidate> {
        let state = self.state.read().await;
        let now = chrono::Utc::now();

        crate::workspace::find_cleanup_candidates(
            state.tasks.values().cloned(),
            now,
            stale_threshold,
        )
    }

    /// Clear the workspace_id from a task after cleanup.
    ///
    /// Called after the container/workspace has been destroyed to update
    /// the task record. Emits a workspace:cleaned event for audit trail.
    pub async fn clear_workspace_id(
        &self,
        task_id: &str,
        reason: &crate::workspace::CleanupReason,
    ) -> Result<(), ServerError> {
        {
            let mut state = self.state.write().await;
            if let Some(task) = state.tasks.get_mut(task_id) {
                let workspace_id = task.workspace_id.take();

                // Write-through to store
                if let Some(ref store) = self.store {
                    if let Ok(store) = store.lock() {
                        if let Err(e) = store.save_task(task) {
                            tracing::error!(
                                task_id = %task_id,
                                error = %e,
                                "failed to persist workspace cleanup to store"
                            );
                        }
                    }
                }

                tracing::info!(
                    task_id = %task_id,
                    workspace_id = ?workspace_id,
                    reason = ?reason,
                    "workspace cleaned up"
                );
            }
        }

        // Emit workspace:cleaned event for audit trail
        let reason_str = match reason {
            crate::workspace::CleanupReason::TerminalState(s) => format!("terminal_state:{:?}", s),
            crate::workspace::CleanupReason::PrMerged => "pr_merged".to_string(),
            crate::workspace::CleanupReason::Stale { idle_duration } => {
                format!("stale:{}s", idle_duration.as_secs())
            }
        };

        let event = Event::new(
            EventType::WorkspaceCleaned,
            task_id,
            Actor::System,
            serde_json::json!({ "reason": reason_str }),
        );
        self.event_bus.publish(event).await?;

        Ok(())
    }

    // --- Lifecycle ---

    /// Emit the system:started event (spec Section 8.3).
    pub async fn emit_started(&self) -> Result<(), ServerError> {
        let event = Event::new(
            EventType::SystemStarted,
            "system",
            Actor::System,
            serde_json::json!({}),
        );
        self.event_bus.publish(event).await?;
        Ok(())
    }

    // --- Rebuild from GitHub (issue #256) ---

    /// Request a rebuild from GitHub.
    ///
    /// This sets a flag that the poll loop will check. When set, the poll loop
    /// will clear its pollers, causing them to be recreated with `since: None`
    /// (which fetches all open items from scratch).
    ///
    /// The rebuild process:
    /// 1. Clear in-memory tasks and merge queue
    /// 2. Clear database tables (preserving accounting and projects)
    /// 3. Set rebuild_requested flag for poll loop
    /// 4. Emit system:rebuild event
    /// 5. Poll loop resets pollers and re-fetches all data
    pub async fn rebuild_from_github(&self) -> Result<RebuildStats, ServerError> {
        let (tasks_cleared, merge_entries_cleared) = {
            // Clear in-memory state
            let mut state = self.state.write().await;
            let tasks_count = state.tasks.len();
            let merge_count = state.merge_queue.entries().len();
            state.tasks.clear();
            state.merge_queue = crate::merge_queue::MergeQueue::new();
            (tasks_count, merge_count)
        };

        // Clear database tables
        let (db_tasks, db_merge) = if let Some(ref store) = self.store {
            let store = store.lock().unwrap();
            let tasks = store.clear_tasks()
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            let merge = store.clear_merge_queue()
                .map_err(|e| ServerError::StoreError(e.to_string()))?;
            (tasks, merge)
        } else {
            (0, 0)
        };

        // Signal poll loop to reset pollers
        self.rebuild_requested.store(true, Ordering::SeqCst);

        // Emit rebuild event
        let event = Event::new(
            EventType::SystemRebuild,
            "system",
            Actor::Human,
            serde_json::json!({
                "tasks_cleared": tasks_cleared,
                "merge_entries_cleared": merge_entries_cleared,
            }),
        );
        self.event_bus.publish(event).await?;

        tracing::info!(
            tasks_cleared = tasks_cleared,
            merge_entries_cleared = merge_entries_cleared,
            db_tasks_cleared = db_tasks,
            db_merge_cleared = db_merge,
            "rebuild from GitHub initiated"
        );

        Ok(RebuildStats {
            tasks_cleared,
            merge_entries_cleared,
        })
    }

    /// Check if a rebuild has been requested.
    ///
    /// Called by the poll loop. Returns true and clears the flag if set.
    pub fn take_rebuild_requested(&self) -> bool {
        self.rebuild_requested.swap(false, Ordering::SeqCst)
    }

    // --- Session failure handling (spec §13.1, §13.2) ---

    /// Handle a task failure with progress detection.
    ///
    /// Implements spec §13.1 and §13.2:
    /// - If `made_progress` is true: the agent ran long enough or made changes,
    ///   so this is considered a transient failure. Reset retry context and
    ///   transition to Waiting for re-dispatch.
    /// - If `made_progress` is false: the agent failed quickly without progress.
    ///   Increment retry_count. If under max_retries, transition to Waiting.
    ///   If retries exhausted, transition to Failed.
    ///
    /// Returns `true` if the task will be retried (transitioned to Waiting),
    /// `false` if it transitioned to Failed.
    ///
    /// The optional `failure_info` parameter contains detailed diagnosis from
    /// the session (spec §13.4). When provided, it is stored with the task for
    /// surfacing in the UI and retry prompt context.
    pub async fn handle_task_failure(
        &self,
        task_id: &str,
        made_progress: bool,
        max_retries: u32,
        failure_info: Option<FailureInfo>,
    ) -> Result<bool, ServerError> {
        let now = chrono::Utc::now();

        let (will_retry, new_state, event_data) = {
            let mut state = self.state.write().await;
            let task = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| ServerError::TaskNotFound(task_id.to_string()))?;

            // Store failure info for UI and retry context (spec §13.4)
            task.last_failure = failure_info.clone();

            if made_progress {
                // Progress was made — this is a transient failure.
                // Reset retry context and transition to Waiting.
                task.retry_count = 0;
                task.last_failure_at = None;
                task.session_id = None;
                task.set_state(TaskState::Waiting);

                // Write-through to store
                if let Some(ref store) = self.store {
                    if let Ok(store) = store.lock() {
                        if let Err(e) = store.save_task(task) {
                            tracing::error!(
                                task_id = %task_id,
                                error = %e,
                                "failed to persist task retry state to store"
                            );
                        }
                    }
                }

                (
                    true,
                    TaskState::Waiting,
                    serde_json::json!({
                        "reason": "session_failure",
                        "made_progress": true,
                        "retry_count_reset": true,
                        "failure_info": failure_info,
                    }),
                )
            } else {
                // No progress made — increment retry count.
                task.retry_count += 1;
                task.last_failure_at = Some(now);
                task.session_id = None;

                if task.retry_count <= max_retries {
                    // Under max retries — transition to Waiting.
                    task.set_state(TaskState::Waiting);

                    // Write-through to store
                    if let Some(ref store) = self.store {
                        if let Ok(store) = store.lock() {
                            if let Err(e) = store.save_task(task) {
                                tracing::error!(
                                    task_id = %task_id,
                                    error = %e,
                                    "failed to persist task retry state to store"
                                );
                            }
                        }
                    }

                    (
                        true,
                        TaskState::Waiting,
                        serde_json::json!({
                            "reason": "session_failure",
                            "made_progress": false,
                            "retry_count": task.retry_count,
                            "failure_info": failure_info,
                        }),
                    )
                } else {
                    // Retries exhausted — transition to Failed.
                    task.set_state(TaskState::Failed);

                    // Write-through to store
                    if let Some(ref store) = self.store {
                        if let Ok(store) = store.lock() {
                            if let Err(e) = store.save_task(task) {
                                tracing::error!(
                                    task_id = %task_id,
                                    error = %e,
                                    "failed to persist task failure to store"
                                );
                            }
                        }
                    }

                    (
                        false,
                        TaskState::Failed,
                        serde_json::json!({
                            "reason": "session_failure",
                            "made_progress": false,
                            "retries_exhausted": true,
                            "retry_count": task.retry_count,
                            "failure_info": failure_info,
                        }),
                    )
                }
            }
        };

        // Emit the appropriate event
        let event_type = match new_state {
            TaskState::Waiting => EventType::TaskStateWaiting,
            TaskState::Failed => EventType::TaskStateFailed,
            _ => unreachable!(),
        };

        let event = Event::new(event_type, task_id, Actor::System, event_data);
        self.event_bus.publish(event).await?;

        Ok(will_retry)
    }

    // --- Restart recovery (spec §13.3) ---

    /// Apply the results of orphan detection to task state.
    ///
    /// For tasks that should retry:
    /// - Increment retry_count
    /// - Set last_failure_at to now
    /// - Clear session_id
    /// - Transition to Waiting
    /// - Emit task:state:waiting event
    ///
    /// For tasks that have exhausted retries:
    /// - Transition to Failed
    /// - Emit task:state:failed event
    pub async fn apply_recovery_result(
        &self,
        result: &crate::recovery::RecoveryResult,
    ) -> Result<(), ServerError> {
        let now = chrono::Utc::now();

        // Handle tasks that should retry
        for task_id in &result.retried {
            {
                let mut state = self.state.write().await;
                if let Some(task) = state.tasks.get_mut(task_id) {
                    task.retry_count += 1;
                    task.last_failure_at = Some(now);
                    task.session_id = None;
                    task.set_state(TaskState::Waiting);

                    // Write-through to store
                    if let Some(ref store) = self.store {
                        if let Ok(store) = store.lock() {
                            if let Err(e) = store.save_task(task) {
                                tracing::error!(
                                    task_id = %task_id,
                                    error = %e,
                                    "failed to persist task recovery to store"
                                );
                            }
                        }
                    }
                }
            }

            // Emit event
            let event = Event::new(
                EventType::TaskStateWaiting,
                task_id,
                Actor::System,
                serde_json::json!({
                    "reason": "orphan_recovery",
                    "retry": true,
                }),
            );
            self.event_bus.publish(event).await?;
        }

        // Handle tasks that have exhausted retries
        for task_id in &result.failed {
            {
                let mut state = self.state.write().await;
                if let Some(task) = state.tasks.get_mut(task_id) {
                    task.last_failure_at = Some(now);
                    task.session_id = None;
                    task.set_state(TaskState::Failed);

                    // Write-through to store
                    if let Some(ref store) = self.store {
                        if let Ok(store) = store.lock() {
                            if let Err(e) = store.save_task(task) {
                                tracing::error!(
                                    task_id = %task_id,
                                    error = %e,
                                    "failed to persist task failure to store"
                                );
                            }
                        }
                    }
                }
            }

            // Emit event
            let event = Event::new(
                EventType::TaskStateFailed,
                task_id,
                Actor::System,
                serde_json::json!({
                    "reason": "orphan_recovery",
                    "retries_exhausted": true,
                }),
            );
            self.event_bus.publish(event).await?;
        }

        Ok(())
    }

    // --- Accounting (spec §16.4) ---

    /// Get the global accounting summary.
    pub fn get_accounting_summary(&self) -> Result<tasks_store::AccountingSummary, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.get_accounting_summary()
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    /// List all task accounting records.
    pub fn list_task_accounting(&self) -> Result<Vec<tasks_store::TaskAccounting>, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.list_accounting()
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    /// Get accounting for a specific task.
    pub fn get_task_accounting(&self, task_id: &str) -> Result<Option<tasks_store::TaskAccounting>, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.get_accounting(task_id)
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    /// Add token usage to a task's accounting.
    pub fn add_task_token_usage(
        &self,
        task_id: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<tasks_store::TaskAccounting, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.add_token_usage(task_id, input_tokens, output_tokens)
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }

    /// Record a session end for a task.
    pub fn record_task_session_end(
        &self,
        task_id: &str,
        duration_seconds: u64,
    ) -> Result<tasks_store::TaskAccounting, ServerError> {
        let store = self.store.as_ref()
            .ok_or_else(|| ServerError::StoreError("store not available".into()))?;
        let store = store.lock().unwrap();
        store.record_session_end(task_id, duration_seconds)
            .map_err(|e| ServerError::StoreError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use events::EventStore;
    use tempfile::tempdir;

    async fn test_server() -> Server {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 64);
        Server::new(bus)
    }

    #[tokio::test]
    async fn default_mode_is_pause() {
        let server = test_server().await;
        assert_eq!(server.mode().await, Mode::Pause);
    }

    #[tokio::test]
    async fn mode_transitions() {
        let server = test_server().await;

        // Human raises to Play
        server.set_mode(Mode::Play, &Actor::Human).await.unwrap();
        assert_eq!(server.mode().await, Mode::Play);

        // Orchestrator lowers to Pause
        server
            .set_mode(Mode::Pause, &Actor::Orchestrator)
            .await
            .unwrap();
        assert_eq!(server.mode().await, Mode::Pause);

        // Orchestrator cannot raise
        assert!(server
            .set_mode(Mode::Play, &Actor::Orchestrator)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn task_lifecycle() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new(
            "task-1",
            crate::model::task::TaskSource::Internal,
            "Test task",
            "proj-1",
        );
        server.add_task(task).await.unwrap();

        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Waiting);

        server
            .set_task_state("task-1", TaskState::Running, Actor::System)
            .await
            .unwrap();

        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Running);
    }

    #[tokio::test]
    async fn task_requires_valid_project() {
        let server = test_server().await;
        let task = Task::new(
            "task-1",
            crate::model::task::TaskSource::Internal,
            "Test task",
            "nonexistent",
        );
        assert!(server.add_task(task).await.is_err());
    }

    #[tokio::test]
    async fn events_emitted() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        server.emit_started().await.unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::SystemStarted);
    }

    #[tokio::test]
    async fn presence_tracking() {
        let server = test_server().await;
        assert!(!server.is_human_present());

        let _guard = server.presence.connect();
        assert!(server.is_human_present());
    }

    #[tokio::test]
    async fn dispatch_respects_stop_mode() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("t1", TaskSource::Internal, "Task", "proj-1");
        server.add_task(task).await.unwrap();

        // Set to Stop mode
        server.set_mode(Mode::Stop, &Actor::Human).await.unwrap();

        let plan = server.run_dispatch(&[], 5).await.unwrap();
        assert!(plan.resume.is_empty());
        assert!(plan.new_work.is_empty());

        // Task should still be Waiting
        let task = server.get_task("t1").await.unwrap();
        assert_eq!(task.state, TaskState::Waiting);
    }

    #[tokio::test]
    async fn dispatch_transitions_waiting_to_running() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("t1", TaskSource::Internal, "Task", "proj-1");
        server.add_task(task).await.unwrap();

        let plan = server.run_dispatch(&[], 5).await.unwrap();
        assert_eq!(plan.new_work, vec!["t1"]);

        let task = server.get_task("t1").await.unwrap();
        assert_eq!(task.state, TaskState::Running);
    }

    #[tokio::test]
    async fn dispatch_respects_concurrency() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // Add 3 tasks
        for i in 1..=3 {
            let task = Task::new(
                format!("t{i}"),
                TaskSource::Internal,
                format!("Task {i}"),
                "proj-1",
            );
            server.add_task(task).await.unwrap();
        }

        // Global max = 2
        let plan = server.run_dispatch(&[], 2).await.unwrap();
        assert_eq!(plan.new_work.len(), 2);

        // Third task should still be Waiting
        let mut running = 0;
        let mut waiting = 0;
        for i in 1..=3 {
            let t = server.get_task(&format!("t{i}")).await.unwrap();
            match t.state {
                TaskState::Running => running += 1,
                TaskState::Waiting => waiting += 1,
                _ => {}
            }
        }
        assert_eq!(running, 2);
        assert_eq!(waiting, 1);
    }

    #[tokio::test]
    async fn dispatch_unblocks_tasks() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // t1 is completed, t2 is blocked by t1
        let mut t1 = Task::new("t1", TaskSource::Internal, "Task 1", "proj-1");
        t1.set_state(TaskState::Completed);
        server.add_task(t1).await.unwrap();

        let mut t2 = Task::new("t2", TaskSource::Internal, "Task 2", "proj-1");
        t2.state = TaskState::Blocked;
        t2.blocked_by = vec!["t1".to_string()];
        server.add_task(t2).await.unwrap();

        let plan = server.run_dispatch(&[], 5).await.unwrap();
        // t2 should have been unblocked and dispatched
        assert!(plan.new_work.contains(&"t2".to_string()));

        let t2 = server.get_task("t2").await.unwrap();
        assert_eq!(t2.state, TaskState::Running);
    }

    #[tokio::test]
    async fn dispatch_resumes_question_tasks() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let mut task = Task::new("t1", TaskSource::Internal, "Task", "proj-1");
        task.set_state(TaskState::Question);
        server.add_task(task).await.unwrap();

        let plan = server
            .run_dispatch(&["t1".to_string()], 5)
            .await
            .unwrap();
        assert_eq!(plan.resume, vec!["t1"]);

        let task = server.get_task("t1").await.unwrap();
        assert_eq!(task.state, TaskState::Running);
    }

    #[tokio::test]
    async fn store_persists_projects_and_tasks() {
        let store = tasks_store::Store::open_memory().unwrap();
        let dir = tempdir().unwrap();
        let event_store = EventStore::new(dir.path());
        let bus = EventBus::new(event_store, 64);
        let server = Server::with_store(bus, store);

        // Add project and task
        let project = Project::new("p1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("t1", TaskSource::Internal, "Test", "p1");
        server.add_task(task).await.unwrap();

        // Verify data is in the store
        let store_ref = server.store.as_ref().unwrap().lock().unwrap();
        assert!(store_ref.get_project("p1").unwrap().is_some());
        assert!(store_ref.get_task("t1").unwrap().is_some());
    }

    #[tokio::test]
    async fn store_persists_state_changes() {
        let store = tasks_store::Store::open_memory().unwrap();
        let dir = tempdir().unwrap();
        let event_store = EventStore::new(dir.path());
        let bus = EventBus::new(event_store, 64);
        let server = Server::with_store(bus, store);

        let project = Project::new("p1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("t1", TaskSource::Internal, "Test", "p1");
        server.add_task(task).await.unwrap();

        server
            .set_task_state("t1", TaskState::Running, Actor::System)
            .await
            .unwrap();

        // Verify the store has the updated state
        let store_ref = server.store.as_ref().unwrap().lock().unwrap();
        let stored_task = store_ref.get_task("t1").unwrap().unwrap();
        assert_eq!(stored_task.state, TaskState::Running);
    }

    #[tokio::test]
    async fn load_from_store_populates_state() {
        // Create a store with pre-existing data
        let store = tasks_store::Store::open_memory().unwrap();
        store
            .save_project(&Project::new("p1", "owner/repo"))
            .unwrap();
        store
            .save_task(&Task::new("t1", TaskSource::Internal, "Test", "p1"))
            .unwrap();

        let dir = tempdir().unwrap();
        let event_store = EventStore::new(dir.path());
        let bus = EventBus::new(event_store, 64);
        let server = Server::with_store(bus, store);

        // State should be empty before load
        assert!(server.get_project("p1").await.is_none());

        // Load from store
        server.load_from_store().await.unwrap();

        // Now state should be populated
        assert!(server.get_project("p1").await.is_some());
        assert!(server.get_task("t1").await.is_some());
    }

    #[tokio::test]
    async fn approve_merge_entry_transitions_and_emits() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        server.add_task(task).await.unwrap();

        let entry = crate::model::merge_queue::MergeQueueEntry::new(
            "mq-1", "task-1", "https://github.com/owner/repo/pull/1",
        );
        server.add_to_merge_queue(entry).await.unwrap();

        // Drain the queued and task-created events
        while rx.try_recv().is_ok() {}

        server.approve_merge_entry("mq-1", "looks good").await.unwrap();

        // Check the entry is approved
        let state = server.state.read().await;
        let entry = state.merge_queue.get("mq-1").unwrap();
        assert_eq!(entry.status, crate::model::merge_queue::MergeStatus::Approved);
        drop(state);

        // Check event was emitted
        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::MergeApproved);
        assert_eq!(event.actor, Actor::Orchestrator);
    }

    #[tokio::test]
    async fn reject_merge_entry_transitions_task_to_waiting() {
        let server = test_server().await;

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.set_state(TaskState::AwaitingMerge);
        server.add_task(task).await.unwrap();

        let entry = crate::model::merge_queue::MergeQueueEntry::new(
            "mq-1", "task-1", "https://github.com/owner/repo/pull/1",
        );
        server.add_to_merge_queue(entry).await.unwrap();

        server
            .reject_merge_entry("mq-1", "needs tests", Some("add unit tests for the new endpoint"))
            .await
            .unwrap();

        // Entry should be rejected
        let state = server.state.read().await;
        let entry = state.merge_queue.get("mq-1").unwrap();
        assert_eq!(entry.status, crate::model::merge_queue::MergeStatus::Rejected);
        drop(state);

        // Task should be back to Waiting for re-dispatch
        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Waiting);
    }

    // --- Progress detection tests (spec §13.1, §13.2) ---

    #[tokio::test]
    async fn handle_task_failure_with_progress_resets_retry_count() {
        let server = test_server().await;

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // Create a task that's already been retried once
        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.retry_count = 1;
        task.last_failure_at = Some(chrono::Utc::now());
        task.set_state(TaskState::Running);
        server.add_task(task).await.unwrap();

        // Handle failure with progress made
        let will_retry = server
            .handle_task_failure("task-1", true, 3, None)
            .await
            .unwrap();

        // Should retry (transition to Waiting)
        assert!(will_retry);

        // Check task state
        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Waiting);
        assert_eq!(task.retry_count, 0); // Reset to 0 because progress was made
        assert!(task.last_failure_at.is_none()); // Cleared because progress was made
    }

    #[tokio::test]
    async fn handle_task_failure_without_progress_increments_retry_count() {
        let server = test_server().await;

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.retry_count = 1;
        task.set_state(TaskState::Running);
        server.add_task(task).await.unwrap();

        // Handle failure without progress
        let will_retry = server
            .handle_task_failure("task-1", false, 3, None)
            .await
            .unwrap();

        // Should retry (under max_retries)
        assert!(will_retry);

        // Check task state
        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Waiting);
        assert_eq!(task.retry_count, 2); // Incremented
        assert!(task.last_failure_at.is_some()); // Set to now
    }

    #[tokio::test]
    async fn handle_task_failure_exhausted_retries_marks_failed() {
        let server = test_server().await;

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // Task at max retries
        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.retry_count = 3; // Already at max
        task.set_state(TaskState::Running);
        server.add_task(task).await.unwrap();

        // Handle failure without progress
        let will_retry = server
            .handle_task_failure("task-1", false, 3, None)
            .await
            .unwrap();

        // Should not retry (retries exhausted)
        assert!(!will_retry);

        // Check task state
        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.retry_count, 4); // Incremented beyond max
        assert!(task.last_failure_at.is_some());
    }

    #[tokio::test]
    async fn handle_task_failure_emits_correct_events() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.set_state(TaskState::Running);
        server.add_task(task).await.unwrap();

        // Drain task-created event
        while rx.try_recv().is_ok() {}

        // Failure with progress → should emit TaskStateWaiting
        server.handle_task_failure("task-1", true, 3, None).await.unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::TaskStateWaiting);
        assert_eq!(event.data["made_progress"], true);
        assert_eq!(event.data["retry_count_reset"], true);
    }

    #[tokio::test]
    async fn handle_task_failure_exhausted_emits_failed_event() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.retry_count = 3; // At max
        task.set_state(TaskState::Running);
        server.add_task(task).await.unwrap();

        // Drain task-created event
        while rx.try_recv().is_ok() {}

        // Failure without progress at max retries → should emit TaskStateFailed
        server.handle_task_failure("task-1", false, 3, None).await.unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::TaskStateFailed);
        assert_eq!(event.data["retries_exhausted"], true);
    }

    #[tokio::test]
    async fn handle_task_failure_nonexistent_task_returns_error() {
        let server = test_server().await;

        // Try to handle failure for a task that doesn't exist
        let result = server.handle_task_failure("nonexistent", true, 3, None).await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServerError::TaskNotFound(_)));
    }

    // --- Orchestrator event tests (spec §8.3) ---

    #[tokio::test]
    async fn emit_orchestrator_feedback_emits_event() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        server
            .emit_orchestrator_feedback("task-1", "please add more tests", Some("merge_rejection"))
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::OrchestratorFeedback);
        assert_eq!(event.task, "task-1");
        assert_eq!(event.actor, Actor::Orchestrator);
        assert_eq!(event.data["feedback"], "please add more tests");
        assert_eq!(event.data["context"], "merge_rejection");
    }

    #[tokio::test]
    async fn emit_orchestrator_feedback_handles_no_context() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        server
            .emit_orchestrator_feedback("task-2", "guidance for task", None)
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::OrchestratorFeedback);
        assert!(event.data["context"].is_null());
    }

    #[tokio::test]
    async fn emit_orchestrator_escalation_emits_event() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        server
            .emit_orchestrator_escalation(
                "task-1",
                "needs_human_decision",
                serde_json::json!({
                    "question": "Should we merge this despite failing CI?",
                    "options": ["merge anyway", "wait for fix"],
                }),
            )
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::OrchestratorEscalation);
        assert_eq!(event.task, "task-1");
        assert_eq!(event.actor, Actor::Orchestrator);
        assert_eq!(event.data["reason"], "needs_human_decision");
        assert_eq!(
            event.data["details"]["question"],
            "Should we merge this despite failing CI?"
        );
    }

    #[tokio::test]
    async fn reject_merge_entry_closed_marks_task_completed() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let mut task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        task.set_state(TaskState::AwaitingMerge);
        server.add_task(task).await.unwrap();

        let entry = crate::model::merge_queue::MergeQueueEntry::new(
            "mq-1", "task-1", "https://github.com/owner/repo/pull/1",
        );
        server.add_to_merge_queue(entry).await.unwrap();

        // Drain queued events
        while rx.try_recv().is_ok() {}

        server
            .reject_merge_entry_closed("mq-1", "issue already closed", TaskState::Completed)
            .await
            .unwrap();

        // Entry should be rejected
        let state = server.state.read().await;
        let entry = state.merge_queue.get("mq-1").unwrap();
        assert_eq!(entry.status, crate::model::merge_queue::MergeStatus::Rejected);
        drop(state);

        // Task should be Completed (not Waiting)
        let task = server.get_task("task-1").await.unwrap();
        assert_eq!(task.state, TaskState::Completed);

        // Check event was emitted with issue_closed flag
        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::MergeRejected);
        assert_eq!(event.data["issue_closed"], true);
    }

    #[tokio::test]
    async fn cleanup_merge_queue_removes_terminal_entries() {
        let server = test_server().await;

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // Add three tasks and queue entries
        for i in 1..=3 {
            let task = Task::new(
                format!("task-{}", i),
                TaskSource::Internal,
                format!("Test task {}", i),
                "proj-1",
            );
            server.add_task(task).await.unwrap();

            let entry = crate::model::merge_queue::MergeQueueEntry::new(
                format!("mq-{}", i),
                format!("task-{}", i),
                format!("https://github.com/owner/repo/pull/{}", i),
            );
            server.add_to_merge_queue(entry).await.unwrap();
        }

        // Mark one as merged, one as rejected, leave one pending
        {
            let mut state = server.state.write().await;
            state.merge_queue.mark_merged("mq-1").unwrap();
            state.merge_queue.reject("mq-2").unwrap();
        }

        // Before cleanup: 3 entries
        {
            let state = server.state.read().await;
            assert_eq!(state.merge_queue.entries().len(), 3);
        }

        // Run cleanup (without any cutoffs for this test)
        server.cleanup_merge_queue(None, None).await;

        // After cleanup: only the pending entry remains
        let state = server.state.read().await;
        assert_eq!(state.merge_queue.entries().len(), 1);
        assert!(state.merge_queue.get("mq-3").is_some());
        assert!(state.merge_queue.get("mq-1").is_none());
        assert!(state.merge_queue.get("mq-2").is_none());
    }

    // --- Branch lookup tests ---

    #[tokio::test]
    async fn find_task_by_branch_new_format_with_unique_suffix() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new(
            "gh-owner-repo-issue-123",
            TaskSource::Internal,
            "Test task",
            "proj-1",
        );
        server.add_task(task).await.unwrap();

        // New format: tasks/{task_id}--{unique_suffix}
        let result = server
            .find_task_by_branch("tasks/gh-owner-repo-issue-123--a1b2c3d4")
            .await;
        assert_eq!(result, Some("gh-owner-repo-issue-123".to_string()));
    }

    #[tokio::test]
    async fn find_task_by_branch_legacy_format_exact_match() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new(
            "gh-owner-repo-issue-123",
            TaskSource::Internal,
            "Test task",
            "proj-1",
        );
        server.add_task(task).await.unwrap();

        // Legacy format: tasks/{task_id}
        let result = server
            .find_task_by_branch("tasks/gh-owner-repo-issue-123")
            .await;
        assert_eq!(result, Some("gh-owner-repo-issue-123".to_string()));
    }

    #[tokio::test]
    async fn find_task_by_branch_no_prefix_returns_none() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        server.add_task(task).await.unwrap();

        // Branch without tasks/ prefix
        let result = server.find_task_by_branch("feature/something").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn find_task_by_branch_unknown_task_returns_none() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // No tasks added
        let result = server
            .find_task_by_branch("tasks/gh-owner-repo-issue-999--abcd1234")
            .await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn find_task_by_branch_does_not_match_partial_task_id() {
        let server = test_server().await;
        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        // Add task with ID "gh-owner-repo-issue-1"
        let task = Task::new(
            "gh-owner-repo-issue-1",
            TaskSource::Internal,
            "Test task",
            "proj-1",
        );
        server.add_task(task).await.unwrap();

        // Branch is for issue-123, should NOT match issue-1
        let result = server
            .find_task_by_branch("tasks/gh-owner-repo-issue-123--abcd1234")
            .await;
        assert_eq!(result, None);
    }

    // --- Terminal state guard tests ---

    #[tokio::test]
    async fn set_task_state_emits_only_one_event_when_called_twice_with_terminal() {
        let server = test_server().await;
        let mut rx = server.event_bus.subscribe();

        let project = Project::new("proj-1", "owner/repo");
        server.add_project(project).await;

        let task = Task::new("t1", TaskSource::Internal, "Test task", "proj-1");
        server.add_task(task).await.unwrap();

        // Drain the TaskCreated event
        let _ = rx.recv().await.unwrap();

        // First call: should apply and emit TaskStateCompleted
        server
            .set_task_state("t1", TaskState::Completed, Actor::System)
            .await
            .unwrap();
        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, EventType::TaskStateCompleted);

        // Second call: task is already terminal, should be a no-op
        server
            .set_task_state("t1", TaskState::Completed, Actor::System)
            .await
            .unwrap();

        // No more events should have been emitted — try_recv should fail
        assert!(
            rx.try_recv().is_err(),
            "expected no second TaskStateCompleted event"
        );

        // Task should still be Completed
        let task = server.get_task("t1").await.unwrap();
        assert_eq!(task.state, TaskState::Completed);
    }

    #[tokio::test]
    async fn remove_project_cascades_to_tasks_and_merge_queue() {
        let server = test_server().await;

        // Add two projects
        let project1 = Project::new("proj-1", "owner/repo1");
        let project2 = Project::new("proj-2", "owner/repo2");
        server.add_project(project1).await;
        server.add_project(project2).await;

        // Add tasks to both projects
        let task1 = Task::new("task-1", TaskSource::Internal, "Task 1", "proj-1");
        let task2 = Task::new("task-2", TaskSource::Internal, "Task 2", "proj-1");
        let task3 = Task::new("task-3", TaskSource::Internal, "Task 3", "proj-2");
        server.add_task(task1).await.unwrap();
        server.add_task(task2).await.unwrap();
        server.add_task(task3).await.unwrap();

        // Add merge queue entries for tasks in proj-1
        let entry1 = crate::model::merge_queue::MergeQueueEntry::new(
            "mq-1", "task-1", "https://github.com/owner/repo1/pull/1",
        );
        let entry2 = crate::model::merge_queue::MergeQueueEntry::new(
            "mq-2", "task-2", "https://github.com/owner/repo1/pull/2",
        );
        server.add_to_merge_queue(entry1).await.unwrap();
        server.add_to_merge_queue(entry2).await.unwrap();

        // Verify initial state
        {
            let state = server.state.read().await;
            assert_eq!(state.projects.len(), 2);
            assert_eq!(state.tasks.len(), 3);
            assert_eq!(state.merge_queue.entries().len(), 2);
        }

        // Remove proj-1
        let removed = server.remove_project("proj-1").await;
        assert!(removed);

        // Verify cascade deletion
        let state = server.state.read().await;
        // Project removed
        assert_eq!(state.projects.len(), 1);
        assert!(state.projects.get("proj-2").is_some());
        assert!(state.projects.get("proj-1").is_none());

        // Tasks for proj-1 removed, task for proj-2 remains
        assert_eq!(state.tasks.len(), 1);
        assert!(state.tasks.get("task-3").is_some());
        assert!(state.tasks.get("task-1").is_none());
        assert!(state.tasks.get("task-2").is_none());

        // Merge queue entries for proj-1 tasks removed
        assert_eq!(state.merge_queue.entries().len(), 0);
    }
}
