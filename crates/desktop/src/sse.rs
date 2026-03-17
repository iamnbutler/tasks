//! Server-Sent Events (SSE) client for real-time event updates.
//!
//! Connects to the Tasks server's `/api/events` endpoint and streams
//! events to the UI. Handles automatic reconnection with a grace period.
//!
//! Reference: web/src/hooks/use-app-state.ts

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use gpui::{AsyncApp, Context, EventEmitter, WeakEntity};
use tracing::{debug, info, warn};

/// Maximum number of events to buffer.
const MAX_EVENTS: usize = 200;

/// Reconnection delay after a connection failure.
const RECONNECT_DELAY_MS: u64 = 1_000;

/// Maximum reconnection delay (exponential backoff cap).
const MAX_RECONNECT_DELAY_MS: u64 = 30_000;

/// Optional filters for the SSE stream.
#[derive(Debug, Clone, Default)]
pub struct SseFilters {
    /// Event type pattern filter (e.g., "task:*", "agent:message").
    pub pattern: Option<String>,
    /// Task ID filter.
    pub task_id: Option<String>,
}

impl SseFilters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.pattern = Some(pattern.into());
        self
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// Build the URL with query parameters.
    fn build_url(&self, base_url: &str) -> String {
        let mut url = format!("{}/api/events", base_url);
        let mut params = Vec::new();

        if let Some(ref pattern) = self.pattern {
            params.push(format!("pattern={}", urlencoding::encode(pattern)));
        }
        if let Some(ref task_id) = self.task_id {
            params.push(format!("task_id={}", urlencoding::encode(task_id)));
        }

        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        url
    }
}

/// Connection state for the SSE client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseConnectionState {
    /// Not connected, initial state.
    Disconnected,
    /// Attempting to connect.
    Connecting,
    /// Connected and receiving events.
    Connected,
    /// Connection lost, within grace period (will auto-reconnect).
    Reconnecting,
    /// Connection failed after grace period.
    Failed,
}

/// Events emitted by the SSE client.
#[derive(Debug, Clone)]
pub enum SseClientEvent {
    /// Connection state changed.
    StateChanged(SseConnectionState),
    /// New event received from server.
    EventReceived(Arc<events::Event>),
    /// Error occurred.
    Error(String),
}

/// SSE client for subscribing to server events.
///
/// This client connects to the Tasks server's `/api/events` endpoint,
/// parses incoming events, and provides them to the UI.
pub struct SseClient {
    /// Base URL of the Tasks server (e.g., "http://localhost:4800").
    base_url: String,
    /// Optional filters for the event stream.
    filters: SseFilters,
    /// Current connection state.
    state: SseConnectionState,
    /// Buffered events (most recent first).
    events: Vec<Arc<events::Event>>,
    /// Flag to signal the background task to stop.
    stop_flag: Option<Arc<AtomicBool>>,
    /// Last error message.
    last_error: Option<String>,
}

impl SseClient {
    /// Create a new SSE client.
    pub fn new(base_url: impl Into<String>, filters: SseFilters) -> Self {
        Self {
            base_url: base_url.into(),
            filters,
            state: SseConnectionState::Disconnected,
            events: Vec::with_capacity(MAX_EVENTS),
            stop_flag: None,
            last_error: None,
        }
    }

    /// Get the current connection state.
    pub fn state(&self) -> SseConnectionState {
        self.state
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.state == SseConnectionState::Connected
    }

    /// Get the buffered events (most recent first).
    pub fn events(&self) -> &[Arc<events::Event>] {
        &self.events
    }

    /// Get the last error message, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Start the SSE connection.
    ///
    /// This spawns a background task that maintains the connection and
    /// emits events through the GPUI event system.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        if self.stop_flag.is_some() {
            // Already connected or connecting
            return;
        }

        self.state = SseConnectionState::Connecting;
        cx.emit(SseClientEvent::StateChanged(self.state));

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop_flag.clone());

        let url = self.filters.build_url(&self.base_url);

        cx.spawn(|this, cx| async move {
            run_sse_loop(url, stop_flag, this, cx).await;
        })
        .detach();
    }

    /// Disconnect from the server.
    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(flag) = self.stop_flag.take() {
            flag.store(true, Ordering::SeqCst);
        }
        self.state = SseConnectionState::Disconnected;
        cx.emit(SseClientEvent::StateChanged(self.state));
    }

    /// Update the connection state.
    fn set_state(&mut self, state: SseConnectionState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.emit(SseClientEvent::StateChanged(state));
            cx.notify();
        }
    }

    /// Add an event to the buffer.
    fn push_event(&mut self, event: events::Event, cx: &mut Context<Self>) {
        let event = Arc::new(event);
        self.events.insert(0, event.clone());
        if self.events.len() > MAX_EVENTS {
            self.events.truncate(MAX_EVENTS);
        }
        cx.emit(SseClientEvent::EventReceived(event));
        cx.notify();
    }

    /// Set error state.
    fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.last_error = Some(error.clone());
        cx.emit(SseClientEvent::Error(error));
        cx.notify();
    }
}

impl EventEmitter<SseClientEvent> for SseClient {}

/// Background task that maintains the SSE connection.
async fn run_sse_loop(
    url: String,
    stop_flag: Arc<AtomicBool>,
    entity: WeakEntity<SseClient>,
    mut cx: AsyncApp,
) {
    let mut reconnect_delay = Duration::from_millis(RECONNECT_DELAY_MS);
    let mut consecutive_failures = 0u32;

    loop {
        // Check for stop signal
        if stop_flag.load(Ordering::SeqCst) {
            info!(url = %url, "SSE client stopped by user");
            break;
        }

        info!(url = %url, "SSE connecting...");

        let (was_connected, result) =
            connect_and_stream(&url, &stop_flag, &entity, &mut cx).await;

        // Reset failure counter if we had a successful connection
        if was_connected {
            consecutive_failures = 0;
            reconnect_delay = Duration::from_millis(RECONNECT_DELAY_MS);
        }

        match result {
            Ok(()) => {
                // Clean disconnect (stop signal received)
                break;
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    url = %url,
                    error = %e,
                    consecutive_failures = consecutive_failures,
                    "SSE connection failed"
                );

                // Update state based on failure count
                let error_msg = e.to_string();
                if let Err(update_err) = entity.update(&mut cx, |client: &mut SseClient, cx| {
                    if consecutive_failures == 1 {
                        // First failure: enter reconnecting state (grace period)
                        client.set_state(SseConnectionState::Reconnecting, cx);
                    } else {
                        // Multiple failures: show as failed
                        client.set_state(SseConnectionState::Failed, cx);
                        client.set_error(error_msg.clone(), cx);
                    }
                }) {
                    warn!(error = %update_err, "Failed to update SSE failure state");
                }

                // Check stop flag before sleeping
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }

                // Exponential backoff
                let delay = reconnect_delay.min(Duration::from_millis(MAX_RECONNECT_DELAY_MS));
                debug!(delay_ms = delay.as_millis(), "SSE reconnecting after delay");

                // Sleep with periodic stop flag checks
                let check_interval = Duration::from_millis(100);
                let mut elapsed = Duration::ZERO;
                while elapsed < delay {
                    if stop_flag.load(Ordering::SeqCst) {
                        info!(url = %url, "SSE client stopped during reconnect");
                        if let Err(e) = entity.update(&mut cx, |client: &mut SseClient, cx| {
                            client.stop_flag = None;
                            client.set_state(SseConnectionState::Disconnected, cx);
                        }) {
                            warn!(error = %e, "Failed to update SSE disconnect state");
                        }
                        return;
                    }
                    smol::Timer::after(check_interval).await;
                    elapsed += check_interval;
                }

                // Increase delay for next attempt (exponential backoff)
                reconnect_delay =
                    (reconnect_delay * 2).min(Duration::from_millis(MAX_RECONNECT_DELAY_MS));
            }
        }
    }

    if let Err(e) = entity.update(&mut cx, |client: &mut SseClient, cx| {
        client.stop_flag = None;
        client.set_state(SseConnectionState::Disconnected, cx);
    }) {
        warn!(error = %e, "Failed to update SSE final disconnect state");
    }
}

/// Connect to the SSE endpoint and stream events.
/// Returns (was_connected, result) — `was_connected` is true if we established a connection
/// before any error, so the caller can reset failure counters.
async fn connect_and_stream(
    url: &str,
    stop_flag: &Arc<AtomicBool>,
    entity: &WeakEntity<SseClient>,
    cx: &mut AsyncApp,
) -> (bool, Result<(), SseError>) {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(SseError::Request);

    let client = match client {
        Ok(c) => c,
        Err(e) => return (false, Err(e)),
    };

    let response = match client
        .get(url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return (false, Err(SseError::Request(e))),
    };

    if !response.status().is_success() {
        return (false, Err(SseError::HttpStatus(response.status().as_u16())));
    }

    // Update state to connected
    if let Err(e) = entity.update(cx, |client: &mut SseClient, cx| {
        client.set_state(SseConnectionState::Connected, cx);
    }) {
        warn!(error = %e, "Failed to update SSE connection state");
    }

    info!(url = %url, "SSE connected");

    // Stream the response body
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    loop {
        // Check for stop signal
        if stop_flag.load(Ordering::SeqCst) {
            return (true, Ok(()));
        }

        // Poll the stream with a timeout to allow checking stop flag
        let timeout = smol::Timer::after(Duration::from_millis(100));
        let next_chunk = stream.next();

        futures::select_biased! {
            chunk = next_chunk.fuse() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        process_buffer(&mut buffer, entity, cx);
                    }
                    Some(Err(e)) => {
                        return (true, Err(SseError::Stream(e.to_string())));
                    }
                    None => {
                        // Stream ended
                        return (true, Err(SseError::StreamEnded));
                    }
                }
            }
            _ = futures::FutureExt::fuse(timeout) => {
                // Timeout - just continue and check stop flag
            }
        }
    }
}

/// Process the SSE buffer, extracting complete events.
fn process_buffer(buffer: &mut String, entity: &WeakEntity<SseClient>, cx: &mut AsyncApp) {
    // Normalize \r\n to \n per SSE spec (WHATWG)
    if buffer.contains("\r\n") {
        *buffer = buffer.replace("\r\n", "\n");
    }

    // SSE format: "data: <json>\n\n"
    while let Some(pos) = buffer.find("\n\n") {
        let message = buffer[..pos].to_string();
        *buffer = buffer[pos + 2..].to_string();

        // Parse SSE message
        for line in message.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                match serde_json::from_str::<events::Event>(data) {
                    Ok(event) => {
                        debug!(event_type = %event.event_type.as_str(), task = %event.task, "SSE event received");
                        if let Err(e) = entity.update(cx, |client: &mut SseClient, cx| {
                            client.push_event(event, cx);
                        }) {
                            warn!(error = %e, "Failed to push SSE event to client");
                        }
                    }
                    Err(e) => {
                        warn!(data = %data, error = %e, "Failed to parse SSE event");
                    }
                }
            }
        }
    }
}

/// SSE-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("HTTP status error: {0}")]
    HttpStatus(u16),
    #[error("Stream error: {0}")]
    Stream(String),
    #[error("Stream ended unexpectedly")]
    StreamEnded,
}

/// URL encoding helper (minimal implementation).
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::with_capacity(s.len() * 3);
        for c in s.chars() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                    result.push(c);
                }
                _ => {
                    for byte in c.to_string().as_bytes() {
                        result.push_str(&format!("%{:02X}", byte));
                    }
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_build_url_no_params() {
        let filters = SseFilters::new();
        let url = filters.build_url("http://localhost:4800");
        assert_eq!(url, "http://localhost:4800/api/events");
    }

    #[test]
    fn filters_build_url_with_pattern() {
        let filters = SseFilters::new().with_pattern("task:*");
        let url = filters.build_url("http://localhost:4800");
        assert_eq!(url, "http://localhost:4800/api/events?pattern=task%3A%2A");
    }

    #[test]
    fn filters_build_url_with_task_id() {
        let filters = SseFilters::new().with_task_id("task-123");
        let url = filters.build_url("http://localhost:4800");
        assert_eq!(url, "http://localhost:4800/api/events?task_id=task-123");
    }

    #[test]
    fn filters_build_url_with_both() {
        let filters = SseFilters::new()
            .with_pattern("agent:*")
            .with_task_id("task-456");
        let url = filters.build_url("http://localhost:4800");
        assert!(url.contains("pattern=agent%3A%2A"));
        assert!(url.contains("task_id=task-456"));
    }

    #[test]
    fn urlencoding_basic() {
        assert_eq!(urlencoding::encode("hello"), "hello");
        assert_eq!(urlencoding::encode("task:*"), "task%3A%2A");
        assert_eq!(urlencoding::encode("a b"), "a%20b");
    }
}
