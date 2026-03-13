//! Server — the platform, spec Section 3.1.
//!
//! The long-running process that hosts the event log, task state,
//! merge queue, scheduler, and serves the web GUI.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;

use events::{Actor, Event, EventBus, EventType};

use crate::merge_queue::MergeQueue;
use crate::mode::{Mode, ModeTransitionError};
use crate::model::project::Project;
use crate::model::task::{Task, TaskState};
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
///
/// Not yet implemented (spec TODOs):
/// - Scheduler (Section 12: dispatch logic TODO)
/// - Web GUI serving (Section 3.1)
/// - Websocket connections (Section 3.1)
/// - Orchestrator integration (Section 4.2)
pub struct Server {
    pub state: Arc<RwLock<ServerState>>,
    pub event_bus: Arc<EventBus>,
    pub presence: Arc<PresenceTracker>,
}

impl Server {
    pub fn new(event_bus: EventBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            event_bus: Arc::new(event_bus),
            presence: Arc::new(PresenceTracker::new()),
        }
    }

    // --- Project management (spec Section 3.3) ---

    pub async fn add_project(&self, project: Project) {
        let mut state = self.state.write().await;
        state.projects.insert(project.id.clone(), project);
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
    pub async fn set_task_state(
        &self,
        task_id: &str,
        new_state: TaskState,
        actor: Actor,
    ) -> Result<(), ServerError> {
        {
            let mut state = self.state.write().await;
            let task = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| ServerError::TaskNotFound(task_id.to_string()))?;
            task.set_state(new_state);
        }

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
}
