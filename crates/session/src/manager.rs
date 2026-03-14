//! Session manager — spec §9, §12.
//!
//! Manages active container sessions. Spawns containers, monitors agent
//! output, maps supervisor events to platform events, and enforces time limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use events::EventBus;
use runtime::{ContainerConfig, ContainerRuntime};

#[derive(Debug, Error)]
pub enum SessionManagerError {
    #[error("session error: {0}")]
    Session(#[from] runtime::SessionError),
    #[error("event store error: {0}")]
    EventStore(#[from] events::StoreError),
    #[error("session already exists for task: {0}")]
    AlreadyExists(String),
    #[error("no session for task: {0}")]
    NotFound(String),
}

/// Commands sent to a running session's monitoring task.
pub(crate) enum SessionCommand {
    /// Deliver a chat message to the agent.
    Chat(String),
    /// Stop the agent process.
    Stop,
}

/// Handle for a running session — tracks metadata and communication channel.
pub struct SessionHandle {
    /// The task this session is executing.
    pub task_id: String,
    /// Container ID for this session.
    pub container_id: String,
    /// When the session started.
    pub started_at: Instant,
    /// Channel to send commands to the monitoring task.
    pub(crate) command_tx: tokio::sync::mpsc::Sender<SessionCommand>,
    /// Handle for the monitoring task.
    pub(crate) monitor_handle: JoinHandle<()>,
}

/// Manages active container sessions (spec §9).
///
/// Generic over `ContainerRuntime` so it can be tested with mocks.
pub struct SessionManager<R: ContainerRuntime> {
    pub(crate) runtime: Arc<R>,
    pub(crate) event_bus: Arc<EventBus>,
    pub(crate) sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    pub(crate) default_config: ContainerConfig,
    /// Timeout waiting for supervisor ready signal.
    pub(crate) ready_timeout: Duration,
    /// Soft time limit — emits escalation event (spec §17.4).
    pub(crate) soft_time_limit: Duration,
    /// Hard time limit — kills session (spec §17.4).
    pub(crate) hard_time_limit: Duration,
    /// Minimum session duration to count as "progress" (spec §13.1).
    pub(crate) progress_threshold: Duration,
}

impl<R: ContainerRuntime + Send + Sync + 'static> SessionManager<R> {
    /// Create a new SessionManager.
    pub fn new(
        runtime: R,
        event_bus: Arc<EventBus>,
        default_config: ContainerConfig,
    ) -> Self {
        Self {
            runtime: Arc::new(runtime),
            event_bus,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            default_config,
            ready_timeout: Duration::from_secs(60),
            soft_time_limit: Duration::from_secs(3600),
            hard_time_limit: Duration::from_secs(4500),
            progress_threshold: Duration::from_secs(60),
        }
    }

    /// Set the ready timeout.
    pub fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// Set the soft time limit (spec §17.4).
    pub fn with_soft_time_limit(mut self, limit: Duration) -> Self {
        self.soft_time_limit = limit;
        self
    }

    /// Set the hard time limit (spec §17.4).
    pub fn with_hard_time_limit(mut self, limit: Duration) -> Self {
        self.hard_time_limit = limit;
        self
    }

    /// Set the progress threshold (spec §13.1).
    pub fn with_progress_threshold(mut self, threshold: Duration) -> Self {
        self.progress_threshold = threshold;
        self
    }

    /// Number of active sessions.
    pub async fn active_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Whether a session exists for the given task.
    pub async fn has_session(&self, task_id: &str) -> bool {
        self.sessions.read().await.contains_key(task_id)
    }

    /// Get the task IDs of all active sessions.
    pub async fn session_ids(&self) -> Vec<String> {
        self.sessions.read().await.keys().cloned().collect()
    }
}
