//! Session manager — spec §9, §12.
//!
//! Manages active container sessions. Spawns containers, monitors agent
//! output, maps supervisor events to platform events, and enforces time limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use events::EventBus;
use models::accounting::TokenUsage;
use runtime::{ContainerConfig, ContainerRuntime};

use crate::token_parser;

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

    /// Get the task ID of the most recently started session (for emergency stop).
    pub async fn newest_session(&self) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .max_by_key(|h| h.started_at)
            .map(|h| h.task_id.clone())
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
            if let Err(e) = handle.command_tx.send(SessionCommand::Stop).await {
                tracing::warn!(task_id = %handle.task_id, error = %e, "failed to send stop command during shutdown");
            }
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
        tracing::info!(task_id = %task_id, container_id = %container_id, "container ready");

        session.start_agent(&repo_url, &branch, &prompt)?;
        tracing::info!(task_id = %task_id, "agent started");

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

/// Tracks accounting state during a session (spec §16.4).
struct SessionAccountingState {
    /// Last reported token usage (to compute deltas from cumulative totals).
    last_token_usage: TokenUsage,
    /// Total token usage for this session.
    total_token_usage: TokenUsage,
    /// When the session started (for duration tracking).
    started_at_utc: chrono::DateTime<Utc>,
}

impl SessionAccountingState {
    fn new() -> Self {
        Self {
            last_token_usage: TokenUsage::default(),
            total_token_usage: TokenUsage::default(),
            started_at_utc: Utc::now(),
        }
    }

    /// Update token usage from a new reading.
    ///
    /// Per spec §13.5, we prefer absolute totals. If the new reading is
    /// cumulative (larger than last), we compute the delta. Otherwise,
    /// we treat it as an increment.
    fn update_tokens(&mut self, new_usage: &TokenUsage) -> TokenUsage {
        // If new totals are >= last totals, this is a cumulative update
        let is_cumulative = new_usage.input_tokens >= self.last_token_usage.input_tokens
            && new_usage.output_tokens >= self.last_token_usage.output_tokens;

        let delta = if is_cumulative {
            // Compute delta from last reported
            let delta = new_usage.delta(&self.last_token_usage);
            self.last_token_usage = new_usage.clone();
            delta
        } else {
            // Treat as incremental (delta-style payload)
            new_usage.clone()
        };

        // Add to total
        self.total_token_usage.add(&delta);

        delta
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
    let mut hard_limit_triggered = false;
    let mut accounting = SessionAccountingState::new();

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
                // Parse token usage from agent output and emit accounting events
                if let Some(token_delta) = handle_token_accounting(&task_id, &supervisor_event, &mut accounting, &event_bus).await {
                    tracing::debug!(
                        task_id = %task_id,
                        input_tokens = token_delta.input_tokens,
                        output_tokens = token_delta.output_tokens,
                        "token usage recorded"
                    );
                }

                // Map and publish platform events
                handle_supervisor_event(&task_id, &supervisor_event, &event_bus).await;

                // Check for agent exit
                if let runtime::protocol::Event::AgentExit(ref exit) = supervisor_event {
                    let ran_long_enough = started_at.elapsed() >= progress_threshold;
                    handle_exit(&task_id, exit, ran_long_enough, &accounting, started_at.elapsed(), &event_bus).await;
                    break;
                }
            }
            Some(cmd) = command_rx.recv() => {
                let mut sess = session_cmd.lock().unwrap();
                match cmd {
                    SessionCommand::Chat(text) => {
                        if let Err(e) = sess.send_chat(text) {
                            tracing::error!(task_id = %task_id, error = %e, "failed to send chat to session");
                        }
                    }
                    SessionCommand::Stop => {
                        if let Err(e) = sess.stop_agent() {
                            tracing::error!(task_id = %task_id, error = %e, "failed to stop agent");
                        }
                    }
                }
            }
            else => break, // both channels closed
        }

        // Time limit checks (spec §17.4)
        let elapsed = started_at.elapsed();
        if elapsed >= hard_limit && !hard_limit_triggered {
            hard_limit_triggered = true;
            tracing::warn!(task_id = %task_id, elapsed_secs = elapsed.as_secs(), "hard time limit reached, terminating session");

            // Emit system:time_limit:hard event
            let hard_event = events::Event::new(
                events::EventType::SystemTimeLimitHard,
                &task_id,
                events::Actor::System,
                serde_json::json!({
                    "elapsed_seconds": elapsed.as_secs(),
                    "hard_limit_seconds": hard_limit.as_secs(),
                }),
            );
            if let Err(e) = event_bus.publish(hard_event).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to publish hard time limit event");
            }

            // Emit task:state:failed event
            let failed_event = events::Event::new(
                events::EventType::TaskStateFailed,
                &task_id,
                events::Actor::System,
                serde_json::json!({
                    "reason": "hard_time_limit",
                    "elapsed_seconds": elapsed.as_secs(),
                    "made_progress": started_at.elapsed() >= progress_threshold,
                }),
            );
            if let Err(e) = event_bus.publish(failed_event).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to publish failed state event");
            }

            // Stop the agent
            let mut sess = session_cmd.lock().unwrap();
            if let Err(e) = sess.stop_agent() {
                tracing::error!(task_id = %task_id, error = %e, "failed to stop agent at hard time limit");
            }
        } else if elapsed >= soft_limit && !soft_limit_notified {
            soft_limit_notified = true;
            tracing::info!(task_id = %task_id, elapsed_secs = elapsed.as_secs(), "soft time limit reached, notifying orchestrator/human");

            // Emit system:time_limit:soft event
            let soft_event = events::Event::new(
                events::EventType::SystemTimeLimitSoft,
                &task_id,
                events::Actor::System,
                serde_json::json!({
                    "elapsed_seconds": elapsed.as_secs(),
                    "soft_limit_seconds": soft_limit.as_secs(),
                    "hard_limit_seconds": hard_limit.as_secs(),
                }),
            );
            if let Err(e) = event_bus.publish(soft_event).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to publish soft time limit event");
            }

            // Emit task:state:question event to notify human/orchestrator
            let question_event = events::Event::new(
                events::EventType::TaskStateQuestion,
                &task_id,
                events::Actor::System,
                serde_json::json!({
                    "reason": "soft_time_limit",
                    "message": "Session has reached the soft time limit. The agent will be terminated at the hard limit unless extended or guided.",
                    "elapsed_seconds": elapsed.as_secs(),
                    "remaining_seconds": (hard_limit.as_secs().saturating_sub(elapsed.as_secs())),
                }),
            );
            if let Err(e) = event_bus.publish(question_event).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to publish question state event");
            }
        }
    }

    // Cleanup: remove from sessions map
    sessions.write().await.remove(&task_id);
    // Abort the reader thread
    reader.abort();
}

/// Parse token usage from agent output and emit accounting events (spec §16.4).
///
/// Returns the token delta if new usage was found.
async fn handle_token_accounting(
    task_id: &str,
    event: &runtime::protocol::Event,
    accounting: &mut SessionAccountingState,
    event_bus: &EventBus,
) -> Option<TokenUsage> {
    // Only process stdout events for token usage
    let data = match event {
        runtime::protocol::Event::AgentStdout(e) => &e.data,
        _ => return None,
    };

    // Try to parse token usage from the output line
    let usage = token_parser::parse_token_usage(data)?;

    // Update accounting state and get the delta
    let delta = accounting.update_tokens(&usage);

    // Only emit event if there's actual token usage
    if delta.input_tokens == 0 && delta.output_tokens == 0 {
        return None;
    }

    // Emit accounting event
    let event_data = serde_json::json!({
        "input_tokens": delta.input_tokens,
        "output_tokens": delta.output_tokens,
        "model": usage.model,
        "cumulative": {
            "input_tokens": accounting.total_token_usage.input_tokens,
            "output_tokens": accounting.total_token_usage.output_tokens,
        }
    });

    let accounting_event = events::Event::new(
        events::EventType::SystemAccountingTokens,
        task_id,
        events::Actor::System,
        event_data,
    );

    if let Err(e) = event_bus.publish(accounting_event).await {
        tracing::error!(task_id = %task_id, error = %e, "failed to publish token accounting event");
    }

    Some(delta)
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
    if let Err(e) = event_bus.publish(platform_event).await {
        tracing::error!(task_id = %task_id, error = %e, "failed to publish supervisor event");
    }
}

/// Handle an agent exit event — publish success or failure and session accounting.
async fn handle_exit(
    task_id: &str,
    exit: &runtime::protocol::AgentExitEvent,
    made_progress: bool,
    accounting: &SessionAccountingState,
    duration: Duration,
    event_bus: &EventBus,
) {
    let success = exit.code == Some(0);

    // Emit task state event
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
    if let Err(e) = event_bus.publish(event).await {
        tracing::error!(task_id = %task_id, error = %e, "failed to publish agent exit event");
    }

    // Emit session accounting event (spec §16.4)
    let session_event = events::Event::new(
        events::EventType::SystemAccountingSession,
        task_id,
        events::Actor::System,
        serde_json::json!({
            "duration_seconds": duration.as_secs(),
            "started_at": accounting.started_at_utc.to_rfc3339(),
            "ended_at": Utc::now().to_rfc3339(),
            "tokens": {
                "input_tokens": accounting.total_token_usage.input_tokens,
                "output_tokens": accounting.total_token_usage.output_tokens,
                "total_tokens": accounting.total_token_usage.total_tokens(),
            },
            "exit_code": exit.code,
            "success": success,
        }),
    );

    if let Err(e) = event_bus.publish(session_event).await {
        tracing::error!(task_id = %task_id, error = %e, "failed to publish session accounting event");
    }

    tracing::info!(
        task_id = %task_id,
        duration_secs = duration.as_secs(),
        input_tokens = accounting.total_token_usage.input_tokens,
        output_tokens = accounting.total_token_usage.output_tokens,
        "session ended"
    );
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
        let accounting = SessionAccountingState::new();
        let duration = Duration::from_secs(60);

        handle_exit("task-1", &exit, false, &accounting, duration, &bus).await;

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
        let accounting = SessionAccountingState::new();
        let duration = Duration::from_secs(60);

        handle_exit("task-1", &exit, false, &accounting, duration, &bus).await;

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
        let accounting = SessionAccountingState::new();
        let duration = Duration::from_secs(60);

        handle_exit("task-1", &exit, true, &accounting, duration, &bus).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateFailed);
        assert_eq!(received.data["made_progress"], true);
    }

    #[tokio::test]
    async fn exit_emits_session_accounting() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(0),
            signal: None,
        };
        let mut accounting = SessionAccountingState::new();
        // Simulate some token usage
        accounting.update_tokens(&TokenUsage::new(1000, 500));
        let duration = Duration::from_secs(3600);

        handle_exit("task-1", &exit, false, &accounting, duration, &bus).await;

        // First event is task state
        let task_event = rx.recv().await.unwrap();
        assert_eq!(task_event.event_type, events::EventType::TaskStateAwaitingMerge);

        // Second event is session accounting
        let session_event = rx.recv().await.unwrap();
        assert_eq!(session_event.event_type, events::EventType::SystemAccountingSession);
        assert_eq!(session_event.data["duration_seconds"], 3600);
        assert_eq!(session_event.data["tokens"]["input_tokens"], 1000);
        assert_eq!(session_event.data["tokens"]["output_tokens"], 500);
        assert_eq!(session_event.data["tokens"]["total_tokens"], 1500);
        assert_eq!(session_event.data["success"], true);
    }

    #[tokio::test]
    async fn token_accounting_parses_and_emits_event() {
        let (bus, mut rx) = test_event_bus().await;
        let mut accounting = SessionAccountingState::new();

        // Simulate agent stdout with token usage
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: r#"{"input_tokens": 1500, "output_tokens": 800}"#.to_string(),
            },
        );

        let delta = handle_token_accounting("task-1", &event, &mut accounting, &bus).await;

        assert!(delta.is_some());
        let delta = delta.unwrap();
        assert_eq!(delta.input_tokens, 1500);
        assert_eq!(delta.output_tokens, 800);

        // Check accounting event was emitted
        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::SystemAccountingTokens);
        assert_eq!(received.data["input_tokens"], 1500);
        assert_eq!(received.data["output_tokens"], 800);
        assert_eq!(received.data["cumulative"]["input_tokens"], 1500);
        assert_eq!(received.data["cumulative"]["output_tokens"], 800);
    }

    #[tokio::test]
    async fn token_accounting_handles_cumulative_updates() {
        let (bus, mut rx) = test_event_bus().await;
        let mut accounting = SessionAccountingState::new();

        // First token usage
        let event1 = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: r#"{"input_tokens": 1000, "output_tokens": 500}"#.to_string(),
            },
        );
        handle_token_accounting("task-1", &event1, &mut accounting, &bus).await;
        let _ = rx.recv().await.unwrap(); // consume first event

        // Cumulative update (larger than previous)
        let event2 = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: r#"{"input_tokens": 1500, "output_tokens": 800}"#.to_string(),
            },
        );
        let delta = handle_token_accounting("task-1", &event2, &mut accounting, &bus).await;

        // Should compute delta from cumulative
        let delta = delta.unwrap();
        assert_eq!(delta.input_tokens, 500);  // 1500 - 1000
        assert_eq!(delta.output_tokens, 300); // 800 - 500

        // Total should be cumulative
        assert_eq!(accounting.total_token_usage.input_tokens, 1500);
        assert_eq!(accounting.total_token_usage.output_tokens, 800);
    }

    #[tokio::test]
    async fn token_accounting_ignores_non_token_output() {
        let (bus, mut rx) = test_event_bus().await;
        let mut accounting = SessionAccountingState::new();

        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "Just some regular output".to_string(),
            },
        );

        let delta = handle_token_accounting("task-1", &event, &mut accounting, &bus).await;

        assert!(delta.is_none());
        // No event should have been published
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn token_accounting_ignores_stderr() {
        let (bus, mut rx) = test_event_bus().await;
        let mut accounting = SessionAccountingState::new();

        let event = runtime::protocol::Event::AgentStderr(
            runtime::protocol::AgentStderrEvent {
                data: r#"{"input_tokens": 1500, "output_tokens": 800}"#.to_string(),
            },
        );

        let delta = handle_token_accounting("task-1", &event, &mut accounting, &bus).await;

        assert!(delta.is_none());
        assert!(rx.try_recv().is_err());
    }
}
