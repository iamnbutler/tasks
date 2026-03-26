//! Session manager — spec §9, §12.
//!
//! Manages active container sessions. Spawns containers, monitors agent
//! output, maps supervisor events to platform events, and enforces time limits.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use events::EventBus;
use models::task::FailureInfo;
use runtime::{ContainerConfig, ContainerRuntime};

/// Maximum number of stderr lines to keep in the rolling buffer.
const MAX_STDERR_LINES: usize = 50;

use crate::accounting::{TokenParser, TokenTracker, TokenUsage};
use crate::interpreter::{emit_signal_events, OutputInterpreter, OutputSignal};

/// Information about an active container session.
///
/// Returned by `SessionManager::container_info()` for the containers view.
#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    /// Container ID (from the container runtime).
    pub container_id: String,
    /// The task this container is executing.
    pub task_id: String,
    /// When the container session started (UTC).
    pub started_at: DateTime<Utc>,
    /// How long the container has been running (seconds).
    pub uptime_secs: u64,
}

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

/// Per-session time limit overrides.
///
/// Used to customize time limits for specific session types (e.g., automation runs
/// have shorter limits than regular task sessions).
#[derive(Debug, Clone, Default)]
pub struct SessionLimits {
    /// Override for soft time limit. If `None`, uses manager's default.
    pub soft_limit: Option<Duration>,
    /// Override for hard time limit. If `None`, uses manager's default.
    pub hard_limit: Option<Duration>,
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
    /// Flag shared with the monitor task to prevent double-destroy.
    /// Whichever side sets this first is responsible for destroying the container.
    pub(crate) destroyed: Arc<AtomicBool>,
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

    /// Get information about all active container sessions.
    ///
    /// Returns a list of `ContainerInfo` structs for the containers view.
    pub async fn container_info(&self) -> Vec<ContainerInfo> {
        let sessions = self.sessions.read().await;
        let now = Instant::now();
        sessions
            .values()
            .map(|handle| {
                let uptime = now.duration_since(handle.started_at);
                // Convert Instant to DateTime<Utc> by subtracting uptime from current time
                let started_at = Utc::now() - chrono::Duration::seconds(i64::try_from(uptime.as_secs()).unwrap_or(i64::MAX));
                ContainerInfo {
                    container_id: handle.container_id.clone(),
                    task_id: handle.task_id.clone(),
                    started_at,
                    uptime_secs: uptime.as_secs(),
                }
            })
            .collect()
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

    /// Stop all sessions and destroy their containers (for clean shutdown).
    ///
    /// Drains the sessions map under a brief write lock, then aborts monitor
    /// tasks and destroys containers without holding the lock.
    pub async fn destroy_all(&self) {
        let entries: Vec<(String, String, JoinHandle<()>, Arc<AtomicBool>)> = {
            let mut sessions = self.sessions.write().await;
            sessions
                .drain()
                .map(|(_, h)| (h.task_id, h.container_id, h.monitor_handle, h.destroyed))
                .collect()
        };

        for (task_id, container_id, monitor_handle, destroyed) in entries {
            // Abort the monitor task first
            monitor_handle.abort();

            // Atomically claim destroy responsibility; skip if the monitor already destroyed it
            if destroyed.swap(true, Ordering::AcqRel) {
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "container already destroyed by monitor task, skipping"
                );
                continue;
            }

            if let Err(e) = self.runtime.destroy(&container_id).await {
                tracing::error!(
                    task_id = %task_id,
                    container_id = %container_id,
                    error = %e,
                    "failed to destroy container during shutdown"
                );
            } else {
                tracing::info!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "destroyed container during shutdown"
                );
            }
        }
    }

    /// Stop all sessions with a timeout, then forcibly destroy remaining containers.
    ///
    /// Spec §6.1: When mode changes to Stop, running agent processes are terminated.
    /// This method first attempts graceful shutdown by sending stop commands to all
    /// sessions, then waits up to `timeout` for them to exit. Any sessions still
    /// running after the timeout are forcibly destroyed.
    ///
    /// Returns the number of sessions that were stopped (both graceful and forced).
    pub async fn stop_all_with_timeout(&self, timeout: Duration) -> usize {
        // Get all session task IDs and their command channels
        let session_info: Vec<(String, String, tokio::sync::mpsc::Sender<SessionCommand>)> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .map(|h| (h.task_id.clone(), h.container_id.clone(), h.command_tx.clone()))
                .collect()
        };

        let count = session_info.len();
        if count == 0 {
            tracing::debug!("no sessions to stop");
            return 0;
        }

        tracing::info!(
            session_count = count,
            timeout_secs = timeout.as_secs(),
            "stopping all sessions with timeout"
        );

        // Phase 0: Emit TaskStateWaiting for all sessions BEFORE stopping them.
        // This ensures tasks return to Waiting state for re-dispatch when mode resumes,
        // regardless of whether the agent exits gracefully (which would otherwise emit
        // TaskStateFailed and consume a retry slot — see spec §6.1).
        for (task_id, container_id, _) in &session_info {
            let event = events::Event::new(
                events::EventType::TaskStateWaiting,
                task_id,
                events::Actor::System,
                serde_json::json!({
                    "reason": "stop_mode",
                    "message": "Session terminated due to Stop mode",
                    "container_id": container_id,
                }),
            );
            if let Err(e) = self.event_bus.publish(event).await {
                tracing::error!(
                    task_id = %task_id,
                    error = %e,
                    "failed to publish TaskStateWaiting before stop"
                );
            }
        }

        // Phase 1: Send stop command to all sessions (graceful shutdown attempt)
        for (task_id, _, command_tx) in &session_info {
            if let Err(e) = command_tx.send(SessionCommand::Stop).await {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "failed to send stop command (session may already be ending)"
                );
            } else {
                tracing::debug!(task_id = %task_id, "sent stop command to session");
            }
        }

        // Phase 2: Wait for sessions to exit gracefully, with timeout
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(100);

        loop {
            let remaining = {
                let sessions = self.sessions.read().await;
                sessions.len()
            };

            if remaining == 0 {
                tracing::info!("all sessions stopped gracefully");
                break;
            }

            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    remaining_sessions = remaining,
                    "timeout reached, forcibly destroying remaining containers"
                );
                break;
            }

            tokio::time::sleep(poll_interval).await;
        }

        // Phase 3: Force-destroy any remaining sessions
        let remaining_entries: Vec<(String, String, JoinHandle<()>, Arc<AtomicBool>)> = {
            let mut sessions = self.sessions.write().await;
            sessions
                .drain()
                .map(|(_, h)| (h.task_id, h.container_id, h.monitor_handle, h.destroyed))
                .collect()
        };

        for (task_id, container_id, monitor_handle, destroyed) in remaining_entries {
            // Abort the monitor task first
            monitor_handle.abort();

            // Atomically claim destroy responsibility; skip if the monitor already destroyed it
            if destroyed.swap(true, Ordering::AcqRel) {
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "container already destroyed by monitor task, skipping"
                );
                continue;
            }

            tracing::warn!(
                task_id = %task_id,
                container_id = %container_id,
                "forcibly destroying container after timeout"
            );

            // TaskStateWaiting was already emitted in Phase 0 for all sessions.
            // Just force-destroy the container here.
            if let Err(e) = self.runtime.destroy(&container_id).await {
                tracing::error!(
                    task_id = %task_id,
                    container_id = %container_id,
                    error = %e,
                    "failed to force-destroy container"
                );
            } else {
                tracing::info!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "force-destroyed container"
                );
            }
        }

        count
    }
}

/// Methods that require `Clone` on the runtime (needed to create `runtime::Session`).
impl<R: ContainerRuntime + Clone + Send + Sync + 'static> SessionManager<R> {
    /// Start a new session for a task (spec §9.1).
    ///
    /// Creates a container, starts the agent, and spawns a monitoring task
    /// that bridges supervisor events to the platform event bus.
    ///
    /// The optional `progress_threshold` allows per-project customization
    /// (spec §13.1, §14.2). If not provided, uses the manager's default.
    ///
    /// The optional `time_limits` allows per-session time limit overrides
    /// (e.g., automation sessions use shorter limits than regular tasks).
    pub async fn start_session(
        &self,
        task_id: String,
        repo_url: String,
        branch: String,
        prompt: String,
        config: Option<ContainerConfig>,
        progress_threshold: Option<Duration>,
    ) -> Result<(), SessionManagerError> {
        self.start_session_with_limits(
            task_id,
            repo_url,
            branch,
            prompt,
            config,
            progress_threshold,
            SessionLimits::default(),
        )
        .await
    }

    /// Start a new session with custom time limits.
    ///
    /// Like `start_session`, but accepts `SessionLimits` to override the
    /// manager's default soft/hard time limits. Used for automation runs
    /// which have shorter time limits than regular task sessions.
    pub async fn start_session_with_limits(
        &self,
        task_id: String,
        repo_url: String,
        branch: String,
        prompt: String,
        config: Option<ContainerConfig>,
        progress_threshold: Option<Duration>,
        time_limits: SessionLimits,
    ) -> Result<(), SessionManagerError> {
        // Check if session already exists
        {
            let sessions = self.sessions.read().await;
            if let Some(handle) = sessions.get(&task_id) {
                if !handle.monitor_handle.is_finished() {
                    return Err(SessionManagerError::AlreadyExists(task_id));
                }
                // Monitor task has finished (panic, OOM-kill, or missed cleanup).
                // Drop the read lock, remove the stale entry, then proceed.
                drop(sessions);
                let removed = self.sessions.write().await.remove(&task_id);
                if let Some(removed) = removed {
                    tracing::warn!(
                        task_id = %task_id,
                        container_id = %removed.container_id,
                        "removed stale session entry (monitor task already finished)"
                    );
                }
            }
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

        // Use per-project progress threshold if provided, else manager's default (spec §14.2)
        let effective_progress_threshold = progress_threshold.unwrap_or(self.progress_threshold);

        // Use per-session time limits if provided, else manager's defaults
        let effective_soft_limit = time_limits.soft_limit.unwrap_or(self.soft_time_limit);
        let effective_hard_limit = time_limits.hard_limit.unwrap_or(self.hard_time_limit);

        // Shared flag to prevent double-destroy between monitor task and destroy_all/stop_all
        let destroyed = Arc::new(AtomicBool::new(false));

        // Spawn the monitoring task
        let monitor = tokio::spawn(monitor_session(
            task_id.clone(),
            session,
            command_rx,
            self.event_bus.clone(),
            self.sessions.clone(),
            self.runtime.clone(),
            container_id.clone(),
            effective_soft_limit,
            effective_hard_limit,
            effective_progress_threshold,
            destroyed.clone(),
        ));

        // Insert handle into sessions map
        let handle = SessionHandle {
            task_id: task_id.clone(),
            container_id,
            started_at: Instant::now(),
            command_tx,
            monitor_handle: monitor,
            destroyed,
        };
        self.sessions.write().await.insert(task_id, handle);

        Ok(())
    }
}

/// Monitoring task that bridges supervisor events to platform events.
///
/// Runs for the lifetime of a session. Reads events from the blocking
/// transport via a dedicated thread, handles commands from the session
/// manager, and enforces time limits. Includes output interpretation (spec §9.3)
/// to detect questions, completion signals, and failure patterns.
async fn monitor_session<R: ContainerRuntime + Send + 'static>(
    task_id: String,
    session: runtime::Session<R>,
    mut command_rx: tokio::sync::mpsc::Receiver<SessionCommand>,
    event_bus: Arc<EventBus>,
    sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    runtime: Arc<R>,
    container_id: String,
    soft_limit: Duration,
    hard_limit: Duration,
    progress_threshold: Duration,
    destroyed: Arc<AtomicBool>,
) {
    let started_at = Instant::now();
    let mut soft_limit_notified = false;
    let mut hard_limit_triggered = false;

    // Compute tokio-compatible deadlines for time limit enforcement.
    // These ensure the select! loop wakes even when no events/commands arrive.
    let tokio_start = tokio::time::Instant::now();
    let soft_deadline = tokio_start + soft_limit;
    let hard_deadline = tokio_start + hard_limit;

    // Output interpreter for state detection (spec §9.3)
    let mut interpreter = OutputInterpreter::new();

    // Token tracker for accounting (spec §16.4)
    let mut token_tracker = TokenTracker::new();

    // Rolling stderr buffer for failure diagnosis (spec §13.4)
    let mut stderr_buffer: VecDeque<String> = VecDeque::with_capacity(MAX_STDERR_LINES);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCommand>();

    // Clone task_id for use in the blocking reader thread
    let task_id_for_reader = task_id.clone();

    // Blocking reader thread — owns the session exclusively.
    // Reads events from the sync transport and processes commands from async tasks,
    // avoiding any cross-boundary Mutex contention.
    let reader = tokio::task::spawn_blocking(move || {
        let mut session = session;
        loop {
            // Drain any pending commands before reading
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    SessionCommand::Chat(text) => {
                        if let Err(e) = session.send_chat(text) {
                            tracing::error!(task_id = %task_id_for_reader, error = %e, "failed to send chat to session");
                        }
                    }
                    SessionCommand::Stop => {
                        if let Err(e) = session.stop_agent() {
                            tracing::error!(task_id = %task_id_for_reader, error = %e, "failed to stop agent");
                        }
                    }
                }
            }

            match session.recv_timeout(Duration::from_secs(1)) {
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
        // Compute the next time-limit deadline so the select! loop wakes even
        // when both channels are idle (fixes #458).
        // Only compute a meaningful deadline if we still have limits to enforce.
        let next_deadline = if !soft_limit_notified {
            Some(soft_deadline)
        } else if !hard_limit_triggered {
            Some(hard_deadline)
        } else {
            None
        };

        tokio::select! {
            Some(supervisor_event) = event_rx.recv() => {
                // Capture stderr into rolling buffer for failure diagnosis (spec §13.4)
                if let runtime::protocol::Event::AgentStderr(ref stderr) = supervisor_event {
                    for line in stderr.data.lines() {
                        if stderr_buffer.len() >= MAX_STDERR_LINES {
                            stderr_buffer.pop_front();
                        }
                        stderr_buffer.push_back(line.to_string());
                    }
                }

                // Map and publish platform events, including output interpretation (spec §9.3)
                // and token accounting (spec §16.4)
                handle_supervisor_event(
                    &task_id,
                    &supervisor_event,
                    &event_bus,
                    &mut interpreter,
                    &mut token_tracker,
                ).await;

                // Check for agent exit
                if let runtime::protocol::Event::AgentExit(ref exit) = supervisor_event {
                    let duration_secs = started_at.elapsed().as_secs();
                    let progress_threshold_secs = progress_threshold.as_secs();
                    let stderr_tail: Vec<String> = stderr_buffer.iter().cloned().collect();
                    let total_tokens = token_tracker.total();
                    handle_exit(
                        &task_id,
                        exit,
                        duration_secs,
                        progress_threshold_secs,
                        stderr_tail,
                        total_tokens,
                        &event_bus,
                    ).await;
                    break;
                }
            }
            Some(cmd) = command_rx.recv() => {
                if cmd_tx.send(cmd).is_err() {
                    tracing::error!(task_id = %task_id, "session reader thread gone, cannot forward command");
                }
            }
            // Timeout arm: fires when the next time limit is reached, even if no
            // events or commands arrive. This ensures hard/soft limits are enforced
            // when the session stalls (#458). The branch guard (if next_deadline.is_some())
            // disables this arm once both limits have been handled, avoiding busy-looping.
            _ = tokio::time::sleep_until(next_deadline.unwrap_or_else(|| tokio::time::Instant::now())), if next_deadline.is_some() => {
                tracing::debug!(task_id = %task_id, "time limit check triggered by timeout");
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
            if cmd_tx.send(SessionCommand::Stop).is_err() {
                tracing::error!(task_id = %task_id, "session reader thread gone, cannot send stop at hard time limit");
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

    // Atomically claim destroy responsibility.
    // If destroy_all or stop_all_with_timeout already set the flag, skip destruction.
    if !destroyed.swap(true, Ordering::AcqRel) {
        match runtime.destroy(&container_id).await {
            Ok(()) => {
                tracing::info!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "destroyed container after session ended"
                );
            }
            Err(e) => {
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    error = %e,
                    "container destroy failed"
                );
            }
        }
    } else {
        tracing::debug!(
            task_id = %task_id,
            container_id = %container_id,
            "container already claimed for destruction by shutdown, skipping"
        );
    }
}

/// Map a supervisor event to a platform event and publish it.
///
/// For stdout events, also runs the output interpreter (spec §9.3) to detect
/// questions, completion signals, and failure patterns, and parses token
/// usage for accounting (spec §16.4).
async fn handle_supervisor_event(
    task_id: &str,
    event: &runtime::protocol::Event,
    event_bus: &EventBus,
    interpreter: &mut OutputInterpreter,
    token_tracker: &mut TokenTracker,
) {
    match event {
        runtime::protocol::Event::AgentStarted(e) => {
            let platform_event = events::Event::new(
                events::EventType::TaskStateRunning,
                task_id,
                events::Actor::Agent,
                serde_json::json!({ "pid": e.pid }),
            );
            if let Err(e) = event_bus.publish(platform_event).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to publish agent started event");
            }
        }
        runtime::protocol::Event::AgentStdout(e) => {
            // Always emit the base agent:message event
            let message_event = events::Event::new(
                events::EventType::AgentMessage,
                task_id,
                events::Actor::Agent,
                serde_json::json!({ "text": e.data }),
            );
            if let Err(err) = event_bus.publish(message_event).await {
                tracing::error!(task_id = %task_id, error = %err, "failed to publish agent message event");
            }

            // Interpret the output for state signals (spec §9.3)
            let signal = interpreter.interpret(&e.data);
            if !matches!(signal, OutputSignal::Message) {
                tracing::debug!(
                    task_id = %task_id,
                    signal = ?signal,
                    "output interpreter detected signal"
                );

                if let Err(err) = emit_signal_events(task_id, &signal, event_bus).await {
                    tracing::error!(task_id = %task_id, error = %err, "failed to emit signal events");
                }
            }

            // Parse token usage for accounting (spec §16.4)
            if let Some((usage, is_cumulative)) = TokenParser::parse(&e.data) {
                // Record in tracker (handles delta computation for cumulative totals)
                token_tracker.record(usage, is_cumulative);

                // Emit accounting event
                let accounting_event = events::Event::new(
                    events::EventType::SystemAccountingTokens,
                    task_id,
                    events::Actor::System,
                    serde_json::json!({
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "is_cumulative": is_cumulative,
                    }),
                );
                if let Err(err) = event_bus.publish(accounting_event).await {
                    tracing::error!(task_id = %task_id, error = %err, "failed to publish accounting event");
                }

                tracing::debug!(
                    task_id = %task_id,
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    is_cumulative = is_cumulative,
                    "recorded token usage"
                );
            }
        }
        runtime::protocol::Event::AgentStderr(e) => {
            let platform_event = events::Event::new(
                events::EventType::AgentMessage,
                task_id,
                events::Actor::Agent,
                serde_json::json!({ "text": e.data, "stream": "stderr" }),
            );
            if let Err(e) = event_bus.publish(platform_event).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to publish agent stderr event");
            }
        }
        // SystemReady and ExecResult are not mapped to platform events
        _ => {}
    }
}

/// Handle an agent exit event — publish success or failure with diagnosis (spec §13.4),
/// and emit session accounting event (spec §16.4).
async fn handle_exit(
    task_id: &str,
    exit: &runtime::protocol::AgentExitEvent,
    duration_secs: u64,
    progress_threshold_secs: u64,
    stderr_tail: Vec<String>,
    total_tokens: TokenUsage,
    event_bus: &EventBus,
) {
    let success = exit.code == Some(0);
    let made_progress = duration_secs >= progress_threshold_secs;

    // Emit session accounting event (spec §16.4)
    let session_accounting_event = events::Event::new(
        events::EventType::SystemAccountingSession,
        task_id,
        events::Actor::System,
        serde_json::json!({
            "duration_seconds": duration_secs,
            "total_input_tokens": total_tokens.input_tokens,
            "total_output_tokens": total_tokens.output_tokens,
            "total_tokens": total_tokens.total(),
            "exit_code": exit.code,
        }),
    );
    if let Err(e) = event_bus.publish(session_accounting_event).await {
        tracing::error!(task_id = %task_id, error = %e, "failed to publish session accounting event");
    }

    tracing::info!(
        task_id = %task_id,
        duration_secs = duration_secs,
        input_tokens = total_tokens.input_tokens,
        output_tokens = total_tokens.output_tokens,
        total_tokens = total_tokens.total(),
        "session ended with token accounting"
    );

    let (event_type, data) = if success {
        (
            events::EventType::TaskStateAwaitingMerge,
            serde_json::json!({ "exit_code": 0 }),
        )
    } else {
        // Classify the failure and include diagnosis info (spec §13.1, §13.4)
        let mut failure_info = FailureInfo::classify(
            exit.code,
            exit.signal.clone(),
            duration_secs,
            progress_threshold_secs,
            stderr_tail,
        );

        // Check stderr for transient patterns (rate limits, network errors)
        failure_info.check_stderr_for_transient_patterns();

        (
            events::EventType::TaskStateFailed,
            serde_json::json!({
                "exit_code": exit.code,
                "signal": exit.signal,
                "made_progress": made_progress,
                "duration_secs": duration_secs,
                "failure_info": failure_info,
            }),
        )
    };

    // Use Actor::Agent since this event represents the agent exiting.
    // This allows the run_loop event handler to process it through
    // handle_task_failure() for retry logic (spec §13.1, §13.2, §18.2).
    let event = events::Event::new(event_type, task_id, events::Actor::Agent, data);
    if let Err(e) = event_bus.publish(event).await {
        tracing::error!(task_id = %task_id, error = %e, "failed to publish agent exit event");
    }
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
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStarted(
            runtime::protocol::AgentStartedEvent { pid: 42 },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateRunning);
        assert_eq!(received.task, "task-1");
        assert_eq!(received.data["pid"], 42);
    }

    #[tokio::test]
    async fn agent_stdout_maps_to_message() {
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "hello world".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::AgentMessage);
        assert_eq!(received.data["text"], "hello world");
    }

    #[tokio::test]
    async fn agent_stderr_maps_to_message() {
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStderr(
            runtime::protocol::AgentStderrEvent {
                data: "error output".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::AgentMessage);
        assert_eq!(received.data["text"], "error output");
        assert_eq!(received.data["stream"], "stderr");
    }

    #[tokio::test]
    async fn system_ready_not_mapped() {
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::SystemReady(
            runtime::protocol::SystemReadyEvent {},
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

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
        let tokens = TokenUsage::new(1000, 500);

        handle_exit("task-1", &exit, 120, 60, vec![], tokens, &bus).await;

        // First event is session accounting
        let accounting = rx.recv().await.unwrap();
        assert_eq!(accounting.event_type, events::EventType::SystemAccountingSession);
        assert_eq!(accounting.data["duration_seconds"], 120);
        assert_eq!(accounting.data["total_input_tokens"], 1000);
        assert_eq!(accounting.data["total_output_tokens"], 500);

        // Second event is the state transition
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

        // Short duration (30s) below progress threshold (60s) -> no progress
        handle_exit("task-1", &exit, 30, 60, vec![], TokenUsage::default(), &bus).await;

        // Skip accounting event
        let _ = rx.recv().await.unwrap();

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

        // Duration (120s) >= progress threshold (60s) -> made progress
        handle_exit("task-1", &exit, 120, 60, vec![], TokenUsage::default(), &bus).await;

        // Skip accounting event
        let _ = rx.recv().await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateFailed);
        assert_eq!(received.data["made_progress"], true);
    }

    #[tokio::test]
    async fn exit_event_uses_agent_actor() {
        // The exit event must use Actor::Agent so run_loop's event handler
        // processes it through handle_task_failure() for retry logic.
        // See spec §13.1, §13.2, §18.2.
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(1),
            signal: None,
        };

        handle_exit("task-1", &exit, 30, 60, vec![], TokenUsage::default(), &bus).await;

        // Skip accounting event (uses System actor)
        let _ = rx.recv().await.unwrap();

        // State change event should use Agent actor
        let received = rx.recv().await.unwrap();
        assert_eq!(received.actor, events::Actor::Agent);
    }

    #[tokio::test]
    async fn exit_includes_failure_info() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(1),
            signal: None,
        };
        let stderr = vec!["error: something went wrong".to_string()];

        handle_exit("task-1", &exit, 30, 60, stderr, TokenUsage::default(), &bus).await;

        // Skip accounting event
        let _ = rx.recv().await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, events::EventType::TaskStateFailed);

        // Check failure_info is present
        let failure_info = &received.data["failure_info"];
        assert_eq!(failure_info["exit_code"], 1);
        assert_eq!(failure_info["duration_secs"], 30);
        assert_eq!(failure_info["failure_type"], "deterministic");
        assert_eq!(failure_info["stderr_tail"][0], "error: something went wrong");
    }

    #[tokio::test]
    async fn exit_detects_transient_rate_limit() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: Some(1),
            signal: None,
        };
        let stderr = vec!["API error: rate limit exceeded (429)".to_string()];

        handle_exit("task-1", &exit, 30, 60, stderr, TokenUsage::default(), &bus).await;

        // Skip accounting event
        let _ = rx.recv().await.unwrap();

        let received = rx.recv().await.unwrap();
        let failure_info = &received.data["failure_info"];
        // Should be upgraded to transient due to rate limit pattern in stderr
        assert_eq!(failure_info["failure_type"], "transient");
    }

    #[tokio::test]
    async fn exit_oom_is_transient() {
        let (bus, mut rx) = test_event_bus().await;
        let exit = runtime::protocol::AgentExitEvent {
            code: None,
            signal: Some("9".to_string()),
        };

        handle_exit("task-1", &exit, 30, 60, vec![], TokenUsage::default(), &bus).await;

        // Skip accounting event
        let _ = rx.recv().await.unwrap();

        let received = rx.recv().await.unwrap();
        let failure_info = &received.data["failure_info"];
        assert_eq!(failure_info["failure_type"], "transient");
        assert!(failure_info["summary"].as_str().unwrap().contains("OOM"));
    }

    #[tokio::test]
    async fn stdout_question_like_text_treated_as_message() {
        // Question detection is disabled (see #415) because output-based pattern
        // matching produced too many false positives. Question-like text should
        // only emit the base agent:message event, no question-related events.
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "Please provide the database connection string.".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        // Should only receive the base agent:message event
        let msg_event = rx.recv().await.unwrap();
        assert_eq!(msg_event.event_type, events::EventType::AgentMessage);

        // No additional events (question detection is disabled)
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stdout_failure_emits_error_event() {
        // When agent output contains failure patterns, we should emit
        // an agent:error event (spec §9.3).
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "I'm stuck and cannot proceed with this task.".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        // First event is the base agent:message
        let msg_event = rx.recv().await.unwrap();
        assert_eq!(msg_event.event_type, events::EventType::AgentMessage);

        // Second event is agent:error
        let error_event = rx.recv().await.unwrap();
        assert_eq!(error_event.event_type, events::EventType::AgentError);
        assert_eq!(error_event.data["source"], "output_interpretation");
    }

    #[tokio::test]
    async fn stdout_completion_emits_completion_hint() {
        // When agent output contains completion patterns, we emit an
        // agent:message with completion_hint=true (spec §9.3).
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "Task completed successfully.".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        // First event is the base agent:message
        let msg_event = rx.recv().await.unwrap();
        assert_eq!(msg_event.event_type, events::EventType::AgentMessage);
        assert_eq!(msg_event.data["text"], "Task completed successfully.");

        // Second event is agent:message with completion_hint
        let hint_event = rx.recv().await.unwrap();
        assert_eq!(hint_event.event_type, events::EventType::AgentMessage);
        assert_eq!(hint_event.data["completion_hint"], true);
    }

    #[tokio::test]
    async fn stdout_normal_message_no_extra_events() {
        // Normal agent output should only emit the base agent:message,
        // no additional signal events.
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: "Reading the configuration file...".to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        // Should receive the base agent:message
        let msg_event = rx.recv().await.unwrap();
        assert_eq!(msg_event.event_type, events::EventType::AgentMessage);

        // No additional events - try_recv should fail
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stdout_with_token_usage_emits_accounting_event() {
        // When agent output contains token usage information, we should emit
        // a system:accounting:tokens event (spec §16.4).
        let (bus, mut rx) = test_event_bus().await;
        let mut interpreter = OutputInterpreter::new();
        let mut token_tracker = TokenTracker::new();
        let event = runtime::protocol::Event::AgentStdout(
            runtime::protocol::AgentStdoutEvent {
                data: r#"{"total_token_usage": {"input_tokens": 1500, "output_tokens": 800}}"#.to_string(),
            },
        );

        handle_supervisor_event("task-1", &event, &bus, &mut interpreter, &mut token_tracker).await;

        // First event is the base agent:message
        let msg_event = rx.recv().await.unwrap();
        assert_eq!(msg_event.event_type, events::EventType::AgentMessage);

        // Second event is the accounting event
        let accounting_event = rx.recv().await.unwrap();
        assert_eq!(accounting_event.event_type, events::EventType::SystemAccountingTokens);
        assert_eq!(accounting_event.data["input_tokens"], 1500);
        assert_eq!(accounting_event.data["output_tokens"], 800);
        assert_eq!(accounting_event.data["is_cumulative"], true);

        // Verify tracker accumulated the tokens
        let total = token_tracker.total();
        assert_eq!(total.input_tokens, 1500);
        assert_eq!(total.output_tokens, 800);
    }
}
