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

    /// Send a chat message to a running session (spec §9.2).
    pub async fn send_chat(&self, task_id: &str, message: String) -> Result<(), SessionManagerError> {
        let sessions = self.sessions.read().await;
        let handle = sessions.get(task_id)
            .ok_or_else(|| SessionManagerError::NotFound(task_id.to_string()))?;
        handle.command_tx.send(SessionCommand::Chat(message)).await
            .map_err(|_| SessionManagerError::NotFound(task_id.to_string()))?;
        Ok(())
    }

    /// Stop a session's agent process (spec §9.5).
    pub async fn stop_session(&self, task_id: &str) -> Result<(), SessionManagerError> {
        let sessions = self.sessions.read().await;
        let handle = sessions.get(task_id)
            .ok_or_else(|| SessionManagerError::NotFound(task_id.to_string()))?;
        handle.command_tx.send(SessionCommand::Stop).await
            .map_err(|_| SessionManagerError::NotFound(task_id.to_string()))?;
        Ok(())
    }

    /// Stop all active sessions (for shutdown).
    pub async fn stop_all(&self) {
        let sessions = self.sessions.read().await;
        for handle in sessions.values() {
            let _ = handle.command_tx.send(SessionCommand::Stop).await;
        }
    }
}

/// Methods that require `Clone` on the runtime (needed to create `runtime::Session`).
impl<R: ContainerRuntime + Clone + Send + Sync + 'static> SessionManager<R> {
    /// Start a new session for a task (spec §9.1).
    ///
    /// Creates a container, starts the agent, and spawns a monitoring task
    /// that bridges supervisor events to the platform event bus.
    pub async fn start_session(
        &self,
        task_id: String,
        repo_url: String,
        branch: String,
        prompt: String,
        config: Option<ContainerConfig>,
    ) -> Result<(), SessionManagerError> {
        // Check if session already exists
        if self.sessions.read().await.contains_key(&task_id) {
            return Err(SessionManagerError::AlreadyExists(task_id));
        }

        // Create and start the runtime session
        let session_config = config.unwrap_or_else(|| self.default_config.clone());
        let runtime_clone = (*self.runtime).clone();
        let mut session = runtime::Session::new(runtime_clone, session_config);

        session.start(self.ready_timeout).await?;
        let container_id = session.container_id().unwrap().to_string();
        eprintln!("[session] {task_id}: container {container_id} ready");

        session.start_agent(&repo_url, &branch, &prompt)?;
        eprintln!("[session] {task_id}: agent started");

        // Create command channel
        let (command_tx, command_rx) = tokio::sync::mpsc::channel::<SessionCommand>(32);

        // Spawn the monitoring task
        let monitor = tokio::spawn(monitor_session(
            task_id.clone(),
            session,
            command_rx,
            self.event_bus.clone(),
            self.sessions.clone(),
            self.soft_time_limit,
            self.hard_time_limit,
            self.progress_threshold,
        ));

        // Insert handle into sessions map
        let handle = SessionHandle {
            task_id: task_id.clone(),
            container_id,
            started_at: Instant::now(),
            command_tx,
            monitor_handle: monitor,
        };
        self.sessions.write().await.insert(task_id, handle);

        Ok(())
    }
}

/// Monitoring task that bridges supervisor events to platform events.
///
/// Runs for the lifetime of a session. Reads events from the blocking
/// transport via a dedicated thread, handles commands from the session
/// manager, and enforces time limits.
async fn monitor_session<R: ContainerRuntime + Send + 'static>(
    task_id: String,
    session: runtime::Session<R>,
    mut command_rx: tokio::sync::mpsc::Receiver<SessionCommand>,
    event_bus: Arc<EventBus>,
    sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    soft_limit: Duration,
    hard_limit: Duration,
    progress_threshold: Duration,
) {
    let started_at = Instant::now();
    let mut soft_limit_notified = false;

    // Wrap session for shared access between blocking recv thread and async command handler
    let session = Arc::new(std::sync::Mutex::new(session));
    let session_recv = session.clone();
    let session_cmd = session.clone();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);

    // Blocking reader thread — reads from the sync transport
    let reader = tokio::task::spawn_blocking(move || {
        loop {
            let result = {
                let sess = session_recv.lock().unwrap();
                sess.recv_timeout(Duration::from_secs(1))
            }; // lock released here
            match result {
                Ok(event) => {
                    if event_tx.blocking_send(event).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    if event_tx.is_closed() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            Some(supervisor_event) = event_rx.recv() => {
                // Map and publish platform events
                handle_supervisor_event(&task_id, &supervisor_event, &event_bus).await;

                // Check for agent exit
                if let runtime::protocol::Event::AgentExit(ref exit) = supervisor_event {
                    let ran_long_enough = started_at.elapsed() >= progress_threshold;
                    handle_exit(&task_id, exit, ran_long_enough, &event_bus).await;
                    break;
                }
            }
            Some(cmd) = command_rx.recv() => {
                let mut sess = session_cmd.lock().unwrap();
                match cmd {
                    SessionCommand::Chat(text) => { let _ = sess.send_chat(text); }
                    SessionCommand::Stop => { let _ = sess.stop_agent(); }
                }
            }
            else => break, // both channels closed
        }

        // Time limit checks
        let elapsed = started_at.elapsed();
        if elapsed >= hard_limit {
            let mut sess = session_cmd.lock().unwrap();
            let _ = sess.stop_agent();
            // The exit event will come through event_rx
        } else if elapsed >= soft_limit && !soft_limit_notified {
            soft_limit_notified = true;
            let event = events::Event::new(
                events::EventType::OrchestratorEscalation,
                &task_id,
                events::Actor::System,
                serde_json::json!({
                    "reason": "session_time_limit",
                    "elapsed_seconds": elapsed.as_secs(),
                    "soft_limit_seconds": soft_limit.as_secs(),
                }),
            );
            let _ = event_bus.publish(event).await;
        }
    }

    // Cleanup: remove from sessions map
    sessions.write().await.remove(&task_id);
    // Abort the reader thread
    reader.abort();
}

/// Map a supervisor event to a platform event and publish it.
async fn handle_supervisor_event(
    task_id: &str,
    event: &runtime::protocol::Event,
    event_bus: &EventBus,
) {
    let (event_type, data) = match event {
        runtime::protocol::Event::AgentStarted(e) => (
            events::EventType::TaskStateRunning,
            serde_json::json!({ "pid": e.pid }),
        ),
        runtime::protocol::Event::AgentStdout(e) => (
            events::EventType::AgentMessage,
            serde_json::json!({ "text": e.data }),
        ),
        runtime::protocol::Event::AgentStderr(e) => (
            events::EventType::AgentMessage,
            serde_json::json!({ "text": e.data, "stream": "stderr" }),
        ),
        // SystemReady and ExecResult are not mapped to platform events
        _ => return,
    };

    let platform_event = events::Event::new(event_type, task_id, events::Actor::Agent, data);
    let _ = event_bus.publish(platform_event).await;
}

/// Handle an agent exit event — publish success or failure.
async fn handle_exit(
    task_id: &str,
    exit: &runtime::protocol::AgentExitEvent,
    made_progress: bool,
    event_bus: &EventBus,
) {
    let success = exit.code == Some(0);

    let (event_type, data) = if success {
        (
            events::EventType::TaskStateAwaitingMerge,
            serde_json::json!({ "exit_code": 0 }),
        )
    } else {
        (
            events::EventType::TaskStateFailed,
            serde_json::json!({
                "exit_code": exit.code,
                "signal": exit.signal,
                "made_progress": made_progress,
            }),
        )
    };

    let event = events::Event::new(event_type, task_id, events::Actor::System, data);
    let _ = event_bus.publish(event).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    /// Create a test EventBus backed by a temp directory.
    async fn test_event_bus() -> (Arc<EventBus>, broadcast::Receiver<Arc<events::Event>>) {
        let dir = tempfile::tempdir().unwrap();
        let store = events::EventStore::new(dir.path());
        let bus = Arc::new(events::EventBus::new(store, 64));
        let rx = bus.subscribe();
        (bus, rx)
    }

    #[tokio::test]
    async fn agent_started_maps_to_running() {
        let (bus, mut rx) = test_event_bus().await;
        let event = runtime::protocol::Event::AgentStarted(
            runtime::protocol::AgentStartedEvent { pid: 42 },
        );

        handle_supervisor_event("task-1", &event, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateRunning);
        assert_eq!(received.task, "task-1");
        assert_eq!(received.data["pid"], 42);
    }

    #[tokio::test]
    async fn agent_stdout_maps_to_message() {
        let (bus, mut rx) = test_event_bus().await;
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "hello world".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::AgentMessage);
        assert_eq!(received.data["text"], "hello world");
    }

    #[tokio::test]
    async fn agent_stderr_maps_to_message() {
        let (bus, mut rx) = test_event_bus().await;
        let event = runtime::protocol::Event::AgentStderr(
            runtime::protocol::AgentStderrEvent {
                data: "error output".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::AgentMessage);
        assert_eq!(received.data["text"], "error output");
        assert_eq!(received.data["stream"], "stderr");
    }

    #[tokio::test]
    async fn system_ready_not_mapped() {
        let (bus, mut rx) = test_event_bus().await;
        let event = runtime::protocol::Event::SystemReady(
            runtime::protocol::SystemReadyEvent {},
        );

        handle_supervisor_event("task-1", &event, &bus).await;

        // No event should have been published — try_recv should fail
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn exit_zero_maps_to_awaiting_merge() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(0),
            signal: None,
        };

        handle_exit("task-1", &exit, false, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateAwaitingMerge);
        assert_eq!(received.data["exit_code"], 0);
    }

    #[tokio::test]
    async fn exit_nonzero_maps_to_failed() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(1),
            signal: None,
        };

        handle_exit("task-1", &exit, false, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateFailed);
        assert_eq!(received.data["exit_code"], 1);
    }

    #[tokio::test]
    async fn exit_includes_progress_flag() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(1),
            signal: None,
        };

        handle_exit("task-1", &exit, true, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateFailed);
        assert_eq!(received.data["made_progress"], true);
    }
}
