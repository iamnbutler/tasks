//! Application state management for the Tasks desktop app.
//!
//! This module implements global state management similar to React Context
//! in the web frontend. It uses GPUI's Model/Entity system for reactive state.
//!
//! Reference: web/src/hooks/use-app-state.ts

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, AsyncApp, Context, Entity, EventEmitter, WeakEntity};
use tracing::{debug, info, warn};

use crate::api::{
    task_state_is_active, ApiClient, ApiError, MergeQueueEntry, Mode, Project, Snapshot, Task,
    TaskState, DEFAULT_SERVER_URL,
};
use crate::SseClient;
use crate::sse::{SseClientEvent, SseConnectionState, SseFilters};

/// Maximum number of events to keep in the buffer.
const MAX_EVENTS: usize = 200;

/// Polling interval for snapshot refresh (milliseconds).
const POLL_INTERVAL_MS: u64 = 5_000;

/// Initial retry delay for snapshot fetch (milliseconds).
const INITIAL_RETRY_DELAY_MS: u64 = 1_000;

/// Maximum retry delay for snapshot fetch (milliseconds).
const MAX_RETRY_DELAY_MS: u64 = 30_000;

/// Check whether an event type string should trigger a snapshot refresh.
/// Matches: task:*, merge:*, system:mode*
fn is_state_changing_event(event_type: &str) -> bool {
    event_type.starts_with("task:")
        || event_type.starts_with("merge:")
        || event_type.starts_with("system:mode")
}

/// Connection status for the app.
///
/// A higher-level abstraction over `SseConnectionState` that could
/// incorporate HTTP polling status in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Not connected to the server.
    Disconnected,
    /// Currently connecting.
    Connecting,
    /// Connected and receiving data.
    Connected,
    /// Connection issues, attempting to reconnect.
    Reconnecting,
    /// Connection failed.
    Failed,
}

impl ConnectionStatus {
    /// Whether we're in a connected or connecting state.
    pub fn is_connected_or_connecting(&self) -> bool {
        matches!(
            self,
            ConnectionStatus::Connected
                | ConnectionStatus::Connecting
                | ConnectionStatus::Reconnecting
        )
    }
}

impl From<SseConnectionState> for ConnectionStatus {
    fn from(state: SseConnectionState) -> Self {
        match state {
            SseConnectionState::Disconnected => ConnectionStatus::Disconnected,
            SseConnectionState::Connecting => ConnectionStatus::Connecting,
            SseConnectionState::Connected => ConnectionStatus::Connected,
            SseConnectionState::Reconnecting => ConnectionStatus::Reconnecting,
            SseConnectionState::Failed => ConnectionStatus::Failed,
        }
    }
}

/// Events emitted by the AppState.
#[derive(Debug, Clone)]
pub enum AppStateEvent {
    /// Snapshot was updated.
    SnapshotUpdated,
    /// Connection status changed.
    ConnectionStatusChanged(ConnectionStatus),
    /// An error occurred.
    Error(String),
    /// New event received from the SSE stream.
    EventReceived(Arc<events::Event>),
    /// Selected project changed.
    SelectedProjectChanged(Option<String>),
}

/// Global application state.
///
/// This is the central state container for the Tasks desktop app.
/// It holds the current snapshot, event buffer, connection status,
/// and provides computed properties for filtering.
pub struct AppState {
    /// Current system snapshot.
    snapshot: Option<Snapshot>,
    /// Event buffer (most recent first). This is the canonical buffer
    /// that the UI reads from; the SSE client's internal buffer is not
    /// exposed to consumers.
    events: Vec<Arc<events::Event>>,
    /// Connection status.
    connection_status: ConnectionStatus,
    /// Last error message.
    last_error: Option<String>,
    /// Currently selected project ID (None = all projects).
    selected_project: Option<String>,
    /// API client for HTTP requests.
    api_client: ApiClient,
    /// SSE client for real-time updates (kept alive for subscriptions).
    #[allow(dead_code)]
    sse_client: Option<Entity<SseClient>>,
    /// Flag to stop the polling loop.
    stop_polling: Option<Arc<AtomicBool>>,
    /// Whether a fetch is currently in flight.
    fetch_in_flight: Arc<AtomicBool>,
    /// Number of consecutive snapshot fetch failures (for backoff).
    snapshot_retry_count: u32,
}

impl AppState {
    /// Create a new AppState with the default server URL.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_server_url(DEFAULT_SERVER_URL, cx)
    }

    /// Create a new AppState with a custom server URL.
    pub fn with_server_url(server_url: impl Into<String>, cx: &mut Context<Self>) -> Self {
        let server_url = server_url.into();
        let api_client = ApiClient::new(&server_url);

        // Create and connect SSE client
        let sse_client = cx.new(|_cx| SseClient::new(&server_url, SseFilters::new()));

        // Subscribe to SSE events
        cx.subscribe(
            &sse_client,
            |this: &mut Self, _entity, event: &SseClientEvent, cx| {
                this.handle_sse_event(event, cx);
            },
        )
        .detach();

        // Start SSE connection
        sse_client.update(cx, |client, cx| {
            client.connect(cx);
        });

        let mut state = Self {
            snapshot: None,
            events: Vec::with_capacity(MAX_EVENTS),
            connection_status: ConnectionStatus::Connecting,
            last_error: None,
            selected_project: None,
            api_client,
            sse_client: Some(sse_client),
            stop_polling: None,
            fetch_in_flight: Arc::new(AtomicBool::new(false)),
            snapshot_retry_count: 0,
        };

        // Start polling loop
        state.start_polling(cx);

        // Fetch initial snapshot
        state.refresh_snapshot(cx);

        state
    }

    // --- Getters ---

    /// Get the current snapshot.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// Get the current operating mode.
    pub fn mode(&self) -> Option<Mode> {
        self.snapshot.as_ref().map(|s| s.mode)
    }

    /// Get all projects.
    pub fn projects(&self) -> &[Project] {
        self.snapshot
            .as_ref()
            .map(|s| s.projects.as_slice())
            .unwrap_or(&[])
    }

    /// Get all tasks.
    pub fn tasks(&self) -> &[Task] {
        self.snapshot
            .as_ref()
            .map(|s| s.tasks.as_slice())
            .unwrap_or(&[])
    }

    /// Get all merge queue entries.
    pub fn merge_queue(&self) -> &[MergeQueueEntry] {
        self.snapshot
            .as_ref()
            .map(|s| s.merge_queue.as_slice())
            .unwrap_or(&[])
    }

    /// Get the event buffer.
    pub fn events(&self) -> &[Arc<events::Event>] {
        &self.events
    }

    /// Get recent events (limited).
    pub fn recent_events(&self, limit: usize) -> &[Arc<events::Event>] {
        let len = self.events.len().min(limit);
        &self.events[..len]
    }

    /// Get the connection status.
    pub fn connection_status(&self) -> ConnectionStatus {
        self.connection_status
    }

    /// Check if connected.
    pub fn is_connected(&self) -> bool {
        self.connection_status == ConnectionStatus::Connected
    }

    /// Get the last error message.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Get the currently selected project ID.
    pub fn selected_project(&self) -> Option<&str> {
        self.selected_project.as_deref()
    }

    /// Get slot utilization info.
    pub fn slot_utilization(&self) -> (u32, u32) {
        self.snapshot
            .as_ref()
            .map(|s| (s.slot_utilization.active, s.slot_utilization.max))
            .unwrap_or((0, 0))
    }

    /// Check if a human is present.
    pub fn human_present(&self) -> bool {
        self.snapshot
            .as_ref()
            .map(|s| s.human_present)
            .unwrap_or(false)
    }

    // --- Computed properties ---

    /// Get tasks filtered by the selected project.
    pub fn filtered_tasks(&self) -> Vec<&Task> {
        let tasks = self.tasks();
        match &self.selected_project {
            Some(project_id) => tasks.iter().filter(|t| &t.project == project_id).collect(),
            None => tasks.iter().collect(),
        }
    }

    /// Get merge queue entries filtered by the selected project.
    pub fn filtered_merge_queue(&self) -> Vec<&MergeQueueEntry> {
        let entries = self.merge_queue();
        match &self.selected_project {
            Some(project_id) => {
                // Get task IDs that belong to the selected project
                let project_task_ids: HashSet<&str> = self
                    .tasks()
                    .iter()
                    .filter(|t| &t.project == project_id)
                    .map(|t| t.id.as_str())
                    .collect();
                entries
                    .iter()
                    .filter(|e| project_task_ids.contains(e.task_id.as_str()))
                    .collect()
            }
            None => entries.iter().collect(),
        }
    }

    /// Get active tasks (Running, Question, Testing).
    ///
    /// "Changes Submitted" (AwaitingMerge state) is NOT active — it means the
    /// agent has finished and the PR is waiting in the merge queue. This
    /// matches the server definition.
    pub fn active_tasks(&self) -> Vec<&Task> {
        self.filtered_tasks()
            .into_iter()
            .filter(|t| task_state_is_active(&t.state))
            .collect()
    }

    /// Count tasks by state.
    pub fn count_by_state(&self, state: TaskState) -> usize {
        self.filtered_tasks()
            .into_iter()
            .filter(|t| t.state == state)
            .count()
    }

    /// Get waiting tasks count.
    pub fn waiting_count(&self) -> usize {
        self.count_by_state(TaskState::Waiting)
    }

    /// Get running tasks count.
    pub fn running_count(&self) -> usize {
        self.count_by_state(TaskState::Running)
    }

    // --- Setters ---

    /// Set the selected project filter.
    pub fn set_selected_project(&mut self, project_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_project != project_id {
            self.selected_project = project_id.clone();
            cx.emit(AppStateEvent::SelectedProjectChanged(project_id));
            cx.notify();
        }
    }

    /// Get the current snapshot retry count.
    pub fn snapshot_retry_count(&self) -> u32 {
        self.snapshot_retry_count
    }

    // --- Actions ---

    /// Manually retry connecting to the server (resets backoff).
    pub fn retry_connection(&mut self, cx: &mut Context<Self>) {
        self.snapshot_retry_count = 0;
        self.connection_status = ConnectionStatus::Connecting;
        cx.emit(AppStateEvent::ConnectionStatusChanged(
            self.connection_status,
        ));
        cx.notify();
        self.refresh_snapshot(cx);
    }

    /// Refresh the snapshot from the server.
    pub fn refresh_snapshot(&mut self, cx: &mut Context<Self>) {
        // Prevent concurrent fetches
        if self
            .fetch_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let api_client = self.api_client.clone();
        let fetch_in_flight = self.fetch_in_flight.clone();

        cx.spawn(async move |this, cx| {
            let result = api_client.fetch_snapshot().await;
            fetch_in_flight.store(false, Ordering::SeqCst);

            if let Err(e) = this.update(cx, |state, cx| {
                state.update_snapshot(result, cx);
            }) {
                warn!(error = %e, "Failed to update snapshot state");
            }
        })
        .detach();
    }

    /// Update the snapshot with a fetch result.
    fn update_snapshot(&mut self, result: Result<Snapshot, ApiError>, cx: &mut Context<Self>) {
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.last_error = None;
                self.snapshot_retry_count = 0;
                // Update connection status if we were previously disconnected
                if self.connection_status == ConnectionStatus::Disconnected
                    || self.connection_status == ConnectionStatus::Failed
                    || self.connection_status == ConnectionStatus::Connecting
                    || self.connection_status == ConnectionStatus::Reconnecting
                {
                    self.connection_status = ConnectionStatus::Connected;
                    cx.emit(AppStateEvent::ConnectionStatusChanged(
                        self.connection_status,
                    ));
                }
                cx.emit(AppStateEvent::SnapshotUpdated);
                cx.notify();
            }
            Err(e) => {
                let had_snapshot = self.snapshot.is_some();
                self.set_error(e.to_string(), cx);

                // If we've never had a snapshot, schedule a retry with backoff
                if !had_snapshot {
                    self.snapshot_retry_count += 1;
                    let delay_ms = INITIAL_RETRY_DELAY_MS
                        .saturating_mul(1 << self.snapshot_retry_count.min(5))
                        .min(MAX_RETRY_DELAY_MS);

                    // Show reconnecting status during retries
                    if self.connection_status != ConnectionStatus::Reconnecting {
                        self.connection_status = ConnectionStatus::Reconnecting;
                        cx.emit(AppStateEvent::ConnectionStatusChanged(
                            self.connection_status,
                        ));
                        cx.notify();
                    }

                    info!(
                        retry_count = self.snapshot_retry_count,
                        delay_ms, "Initial snapshot fetch failed, retrying"
                    );

                    let delay = Duration::from_millis(delay_ms);
                    cx.spawn(async move |this, cx| {
                        smol::Timer::after(delay).await;
                        if let Err(e) = this.update(cx, |state, cx| {
                            state.refresh_snapshot(cx);
                        }) {
                            warn!(error = %e, "Failed to trigger snapshot retry");
                        }
                    })
                    .detach();
                }
            }
        }
    }

    /// Set an error state.
    fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        warn!(error = %error, "AppState error");
        self.last_error = Some(error.clone());
        cx.emit(AppStateEvent::Error(error));
        cx.notify();
    }

    /// Add an event to the buffer.
    fn push_event(&mut self, event: events::Event, cx: &mut Context<Self>) {
        let event = Arc::new(event);
        self.events.insert(0, event.clone());
        if self.events.len() > MAX_EVENTS {
            self.events.truncate(MAX_EVENTS);
        }
        cx.emit(AppStateEvent::EventReceived(event));
        cx.notify();
    }

    // --- Polling ---

    /// Start the polling loop for snapshot refresh.
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.stop_polling.is_some() {
            return;
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_polling = Some(stop_flag.clone());

        cx.spawn(async move |this, cx| {
            run_polling_loop(stop_flag, this, cx).await;
        })
        .detach();
    }

    /// Stop the polling loop.
    pub fn stop_polling(&mut self) {
        if let Some(flag) = self.stop_polling.take() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    // --- SSE event handling ---

    /// Handle events from the SSE client.
    fn handle_sse_event(&mut self, event: &SseClientEvent, cx: &mut Context<Self>) {
        match event {
            SseClientEvent::StateChanged(state) => {
                self.connection_status = ConnectionStatus::from(*state);
                cx.emit(AppStateEvent::ConnectionStatusChanged(
                    self.connection_status,
                ));
                cx.notify();
            }
            SseClientEvent::EventReceived(event) => {
                // Check if this is a state-changing event
                let event_type_str = event.event_type.as_str();
                if is_state_changing_event(event_type_str) {
                    debug!(event_type = %event_type_str, "State-changing event, refreshing snapshot");
                    self.refresh_snapshot(cx);
                }

                // Clone and push the event into our buffer.
                // The SSE client also buffers events internally, but AppState's
                // buffer is the authoritative source for the UI.
                let event_clone = events::Event {
                    id: event.id,
                    event_type: event.event_type.clone(),
                    task: event.task.clone(),
                    actor: event.actor.clone(),
                    ts: event.ts,
                    data: event.data.clone(),
                };
                self.push_event(event_clone, cx);
            }
            SseClientEvent::Error(error) => {
                self.set_error(error.clone(), cx);
            }
        }
    }
}

impl EventEmitter<AppStateEvent> for AppState {}

impl Drop for AppState {
    fn drop(&mut self) {
        self.stop_polling();
    }
}

/// Background polling loop for snapshot refresh.
async fn run_polling_loop(
    stop_flag: Arc<AtomicBool>,
    entity: WeakEntity<AppState>,
    cx: &mut AsyncApp,
) {
    let interval = Duration::from_millis(POLL_INTERVAL_MS);

    loop {
        // Sleep first (initial fetch is done in constructor)
        smol::Timer::after(interval).await;

        // Check for stop signal
        if stop_flag.load(Ordering::SeqCst) {
            info!("Polling loop stopped");
            break;
        }

        // Trigger a snapshot refresh
        if let Err(e) = entity.update(cx, |state, cx| {
            state.refresh_snapshot(cx);
        }) {
            warn!(error = %e, "Failed to trigger snapshot refresh from polling loop");
        }
    }
}

/// Create the app state with the default or environment-configured server URL.
pub fn create_app_state(cx: &mut Context<AppState>) -> AppState {
    let server_url =
        std::env::var("TASKS_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());
    AppState::with_server_url(server_url, cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_state_changing_event_matches() {
        assert!(is_state_changing_event("task:created"));
        assert!(is_state_changing_event("task:state:running"));
        assert!(is_state_changing_event("merge:approved"));
        assert!(is_state_changing_event("system:mode:play"));
        assert!(!is_state_changing_event("agent:message"));
        assert!(!is_state_changing_event("human:message"));
    }

    #[test]
    fn connection_status_is_connected_or_connecting() {
        assert!(ConnectionStatus::Connected.is_connected_or_connecting());
        assert!(ConnectionStatus::Connecting.is_connected_or_connecting());
        assert!(ConnectionStatus::Reconnecting.is_connected_or_connecting());
        assert!(!ConnectionStatus::Disconnected.is_connected_or_connecting());
        assert!(!ConnectionStatus::Failed.is_connected_or_connecting());
    }

    #[test]
    fn connection_status_from_sse_state() {
        assert_eq!(
            ConnectionStatus::from(SseConnectionState::Connected),
            ConnectionStatus::Connected
        );
        assert_eq!(
            ConnectionStatus::from(SseConnectionState::Failed),
            ConnectionStatus::Failed
        );
    }
}
