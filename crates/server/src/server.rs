//! Server — the platform, spec Section 3.1.
//!
//! The long-running process that hosts the event log, task state,
//! merge queue, scheduler, and serves the web GUI.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use thiserror::Error;
use tokio::sync::RwLock;

use events::{Actor, Event, EventBus, EventType};

use crate::merge_queue::MergeQueue;
use crate::mode::{Mode, ModeTransitionError};
use crate::model::project::Project;
use crate::model::task::{Task, TaskSource, TaskState};
use crate::dispatcher::{self, DispatchPlan};
use crate::presence::PresenceTracker;

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
    #[error("store error: {0}")]
    StoreError(String),
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
}

impl ServerState {
    fn new() -> Self {
        Self {
            mode: Mode::Pause,
            projects: HashMap::new(),
            tasks: HashMap::new(),
            merge_queue: MergeQueue::new(),
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
}

impl Server {
    pub fn new(event_bus: EventBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            event_bus: Arc::new(event_bus),
            presence: Arc::new(PresenceTracker::new()),
            store: None,
        }
    }

    /// Create a server with persistent storage (spec Section 3.5).
    pub fn with_store(event_bus: EventBus, store: tasks_store::Store) -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            event_bus: Arc::new(event_bus),
            presence: Arc::new(PresenceTracker::new()),
            store: Some(Arc::new(StdMutex::new(store))),
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
            if let Some(ref store) = self.store {
                if let Ok(store) = store.lock() {
                    if let Err(e) = store.delete_project(id) {
                        tracing::error!(project_id = %id, error = %e, "failed to delete project from store");
                    }
                }
            }
        }
        removed
    }

    pub async fn get_project(&self, id: &str) -> Option<Project> {
        let state = self.state.read().await;
        state.projects.get(id).cloned()
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
    pub async fn set_task_state(
        &self,
        task_id: &str,
        new_state: TaskState,
        actor: Actor,
    ) -> Result<(), ServerError> {
        self.apply_task_state(task_id, new_state).await?;

        let event_type = match new_state {
            TaskState::Waiting => EventType::TaskStateWaiting,
            TaskState::Blocked => EventType::TaskStateBlocked,
            TaskState::Running => EventType::TaskStateRunning,
            TaskState::Question => EventType::TaskStateQuestion,
            TaskState::Testing => EventType::TaskStateTesting,
            TaskState::AwaitingMerge => EventType::TaskStateAwaitingMerge,
            TaskState::Conflict => EventType::TaskStateConflict,
            TaskState::Completed => EventType::TaskStateCompleted,
            TaskState::Failed => EventType::TaskStateFailed,
            TaskState::Cancelled => EventType::TaskStateCancelled,
        };

        let event = Event::new(event_type, task_id, actor, serde_json::json!({}));
        self.event_bus.publish(event).await?;
        Ok(())
    }

    /// Update task state and persist to store, **without publishing an event**.
    ///
    /// Only call this from the event-handler loop in `app`, where the
    /// state-change event was already published by the session monitor.
    /// All other callers should use [`set_task_state`], which publishes
    /// the corresponding event.
    pub async fn apply_task_state(
        &self,
        task_id: &str,
        new_state: TaskState,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let task = state
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| ServerError::TaskNotFound(task_id.to_string()))?;
        task.set_state(new_state);

        // Write-through to store
        if let Some(ref store) = self.store {
            if let Ok(store) = store.lock() {
                if let Err(e) = store.save_task(task) {
                    tracing::error!(task_id = %task_id, error = %e, "failed to persist task state to store");
                }
            }
        }
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
        {
            let mut state = self.state.write().await;
            if state.merge_queue.get_by_task(&task_id).is_some() {
                return Ok(()); // Already queued
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
                            .map_or(false, |dep| dep.state.is_terminal())
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

        // 3. Build project_limits map (for now, use global_max for all projects).
        let project_limits = {
            let state = self.state.read().await;
            let mut limits = HashMap::new();
            for project_id in state.projects.keys() {
                limits.insert(project_id.clone(), global_max);
            }
            limits
        };

        // 4. Call dispatcher::evaluate() with current tasks.
        let plan = {
            let state = self.state.read().await;
            dispatcher::evaluate(&state.tasks, pending_answers, &project_limits, global_max)
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

    /// Flush the merge queue (spec Section 6.2).
    ///
    /// Only valid in Pause mode.
    pub async fn flush_merge_queue(&self) -> Result<Vec<String>, ServerError> {
        let mut state = self.state.write().await;
        let mode = state.mode;
        let flushed = state
            .merge_queue
            .flush(mode)
            .map_err(|e| ServerError::EventStore(events::StoreError::Io(
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
            )))?;

        drop(state);

        // Emit flush event
        let event = Event::new(
            EventType::SystemFlush,
            "system",
            Actor::Human,
            serde_json::json!({ "flushed": flushed }),
        );
        self.event_bus.publish(event).await?;

        Ok(flushed)
    }

    // --- Presence (spec Section 4.1) ---

    /// Whether the human is present (has active GUI connections).
    pub fn is_human_present(&self) -> bool {
        self.presence.is_present()
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
}
