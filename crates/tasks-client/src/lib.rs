//! Typed blocking client for the tasks HTTP API.
//!
//! Blocking on purpose: the API is loopback-only plain http, so calls are
//! sub-millisecond and there is nothing to overlap — a GUI client runs them
//! on its own worker threads (gpui's background executor) without dragging
//! an async runtime into the app. Streams ([`Client::stream_events`] and
//! friends) block their thread between frames; give each one a dedicated
//! thread.
//!
//! Types come from `tasks-api` — the same definitions the server serializes,
//! so there is no hand-mirrored wire layer and version skew is a build error.

mod sse;

pub use sse::{EventStream, EventStreamItem, OrchestratorFeed, TranscriptTail};
pub use tasks_api as api;

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tasks_api::events::Event;
use tasks_api::http::{
    BriefingStatus, BuildDetail, BuildRequest, CaptureIssue, CloseTaskRequest, CreateProject,
    ErrorResponse, ModeResponse, ReorderQueue, ReorderSpecQueue, ReviewRequest, SendMessage,
    SetCharter, SetMode,
};
use tasks_api::models::{
    Build, BuildId, Capability, CharterEntry, CharterLevel, CloseReason, Mode, OrchestratorMessage,
    OrchestratorSessionInfo, Project, Session, SessionId, Spec, SpecId, SpecQueueItem,
    SpecQueueStatus, Task, TaskId, TranscriptLine,
};
use thiserror::Error;

/// The server's default port (`TASKS_SERVER_PORT`).
pub const DEFAULT_PORT: u16 = 4800;

/// Overall budget for one API call. Loopback answers in microseconds; a call
/// that takes seconds means the server is wedged, and hanging a GUI worker
/// on it helps nobody.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Read timeout for SSE streams. The server sends a keep-alive comment every
/// 15s, so 60s of silence means the connection is dead, not idle.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum ClientError {
    /// The server answered non-2xx; `message` is its `{"error"}` body.
    #[error("api error ({status}): {message}")]
    Api { status: u16, message: String },
    /// The request never got an HTTP answer (refused, reset, timed out).
    #[error("transport: {0}")]
    Transport(String),
    /// The response body didn't parse as the expected type — a version-skewed
    /// server, given that both ends ship these types from one repo.
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl ClientError {
    /// Errors that retrying the same request cannot fix (the server answered
    /// and said no). Streams stop on these instead of reconnect-looping.
    fn is_terminal(&self) -> bool {
        matches!(self, ClientError::Api { .. })
    }
}

pub type Result<T, E = ClientError> = std::result::Result<T, E>;

fn map_ureq(err: ureq::Error) -> ClientError {
    match err {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            let message = serde_json::from_str::<ErrorResponse>(&body)
                .map(|e| e.error)
                .unwrap_or(body);
            ClientError::Api { status, message }
        }
        ureq::Error::Transport(transport) => ClientError::Transport(transport.to_string()),
    }
}

/// A handle on one tasks server. Cheap to clone (the connection pools are
/// shared); all methods are blocking.
#[derive(Clone)]
pub struct Client {
    base: String,
    /// Agent for plain calls, with an overall timeout.
    calls: ureq::Agent,
    /// Agent for SSE streams: no overall timeout (streams are open-ended),
    /// read timeout tuned to the server's keep-alive cadence.
    pub(crate) streams: ureq::Agent,
}

impl Client {
    pub fn new(port: u16) -> Self {
        Self::with_base(format!("http://127.0.0.1:{port}"))
    }

    /// Base URL like `http://127.0.0.1:4800` (trailing slashes tolerated).
    pub fn with_base(base: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            base,
            calls: ureq::AgentBuilder::new().timeout(CALL_TIMEOUT).build(),
            streams: ureq::AgentBuilder::new()
                .timeout_read(STREAM_READ_TIMEOUT)
                .build(),
        }
    }

    /// Port from `TASKS_SERVER_PORT` — the same variable the server reads —
    /// falling back to [`DEFAULT_PORT`].
    pub fn from_env() -> Self {
        let port = std::env::var("TASKS_SERVER_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        Self::new(port)
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    // --- plumbing ---

    fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let mut request = self.calls.get(&self.url(path));
        for (key, value) in query {
            request = request.query(key, value);
        }
        Ok(request.call().map_err(map_ureq)?.into_json()?)
    }

    fn post_json<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        Ok(self
            .calls
            .post(&self.url(path))
            .send_json(body)
            .map_err(map_ureq)?
            .into_json()?)
    }

    /// A write whose only answer is "accepted" — no body to decode.
    fn post_json_accepted<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.calls
            .post(&self.url(path))
            .send_json(body)
            .map_err(map_ureq)?;
        Ok(())
    }

    fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .calls
            .post(&self.url(path))
            .call()
            .map_err(map_ureq)?
            .into_json()?)
    }

    // --- projects ---

    pub fn projects(&self) -> Result<Vec<Project>> {
        self.get_json("/projects", &[])
    }

    pub fn create_project(
        &self,
        repo_owner: impl Into<String>,
        repo_name: impl Into<String>,
    ) -> Result<Project> {
        self.post_json(
            "/projects",
            &CreateProject {
                repo_owner: repo_owner.into(),
                repo_name: repo_name.into(),
            },
        )
    }

    // --- tasks ---

    /// The working set, in queue order (closed-intake noise hidden).
    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.get_json("/tasks", &[])
    }

    /// Every row, including retired-behind-closed-issue ones.
    pub fn all_tasks(&self) -> Result<Vec<Task>> {
        self.get_json("/tasks", &[("all", "true".into())])
    }

    /// Serves retired tasks too — the title lookup for old builds.
    pub fn task(&self, id: &TaskId) -> Result<Task> {
        self.get_json(&format!("/tasks/{id}"), &[])
    }

    pub fn queue_task(&self, id: &TaskId) -> Result<Task> {
        self.post_empty(&format!("/tasks/{id}/queue"))
    }

    pub fn dequeue_task(&self, id: &TaskId) -> Result<Task> {
        self.post_empty(&format!("/tasks/{id}/dequeue"))
    }

    /// "Scout now": queue at the front; the dispatch loop picks it up.
    pub fn scout_task_now(&self, id: &TaskId) -> Result<Task> {
        self.post_empty(&format!("/tasks/{id}/scout"))
    }

    /// File an issue upstream and track it. Lands in the backlog — capturing
    /// work and queueing it are separate steps.
    pub fn capture_issue(&self, issue: CaptureIssue) -> Result<Task> {
        self.post_json("/issues", &issue)
    }

    /// Close the GitHub issue behind a task.
    ///
    /// 202, and nothing to apply: the task is not retired here. The poller
    /// observes the closure on its next pass, exactly as it would for an issue
    /// closed in a browser.
    pub fn close_task(
        &self,
        id: &TaskId,
        reason: CloseReason,
        rationale: Option<String>,
    ) -> Result<()> {
        self.post_json_accepted(
            &format!("/tasks/{id}/close"),
            &CloseTaskRequest {
                reason: reason.as_str().to_string(),
                rationale,
                evidence: None,
            },
        )
    }

    // --- charter ---

    /// What the orchestrator may currently do.
    pub fn charter(&self) -> Result<Vec<CharterEntry>> {
        self.get_json("/charter", &[])
    }

    /// Set one capability's standing. Human-only at the server.
    pub fn set_charter(
        &self,
        capability: Capability,
        level: CharterLevel,
        daily_limit: Option<i64>,
    ) -> Result<CharterEntry> {
        self.post_json(
            &format!("/charter/{}", capability.as_str()),
            &SetCharter {
                level: level.as_str().to_string(),
                daily_limit,
            },
        )
    }

    /// `task_ids` is the complete queue order, front to back. Returns the
    /// same projection as [`Client::tasks`] — apply it in place of the list.
    pub fn reorder_queue(&self, task_ids: Vec<TaskId>) -> Result<Vec<Task>> {
        self.post_json("/queue/reorder", &ReorderQueue { task_ids })
    }

    // --- sessions & transcripts ---

    pub fn sessions(&self) -> Result<Vec<Session>> {
        self.get_json("/sessions", &[])
    }

    pub fn session(&self, id: &SessionId) -> Result<Session> {
        self.get_json(&format!("/sessions/{id}"), &[])
    }

    /// Catch-up read. `since` is inclusive — a tailing caller passes
    /// `last_seq + 1`. Prefer [`Client::stream_transcript`] for live tails.
    pub fn transcript(
        &self,
        id: &SessionId,
        since: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<TranscriptLine>> {
        let mut query = Vec::new();
        if let Some(since) = since {
            query.push(("since", since.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        self.get_json(&format!("/sessions/{id}/transcript"), &query)
    }

    // --- specs & the review queue ---

    pub fn specs(&self) -> Result<Vec<Spec>> {
        self.get_json("/specs", &[])
    }

    pub fn spec(&self, id: &SpecId) -> Result<Spec> {
        self.get_json(&format!("/specs/{id}"), &[])
    }

    pub fn spec_queue(&self) -> Result<Vec<SpecQueueItem>> {
        self.get_json("/spec-queue", &[])
    }

    pub fn reorder_spec_queue(&self, spec_ids: Vec<SpecId>) -> Result<Vec<SpecQueueItem>> {
        self.post_json("/spec-queue/reorder", &ReorderSpecQueue { spec_ids })
    }

    /// Render a verdict. The server accepts `Approved`, `NeedsRevision`
    /// (feedback is what the re-scout sees) and `Rejected`; anything else is
    /// its 400, not ours.
    pub fn review_spec(
        &self,
        id: &SpecId,
        verdict: SpecQueueStatus,
        feedback: Option<String>,
    ) -> Result<SpecQueueItem> {
        self.post_json(
            &format!("/spec-queue/{id}/review"),
            &ReviewRequest {
                status: verdict.as_str().to_string(),
                feedback,
                // The client speaks for the human, who owes no explanation.
                rationale: None,
                evidence: None,
            },
        )
    }

    // --- builds ---

    /// Newest first.
    pub fn builds(&self) -> Result<Vec<Build>> {
        self.get_json("/builds", &[])
    }

    /// The build joined with its batch's spec ids, in position order.
    pub fn build(&self, id: &BuildId) -> Result<BuildDetail> {
        self.get_json(&format!("/builds/{id}"), &[])
    }

    /// 202: queued, not started — builds are serial. Watch `build_started` /
    /// `build_completed` events. `base_branch` defaults to `main`.
    pub fn request_build(
        &self,
        spec_ids: Vec<SpecId>,
        base_branch: Option<String>,
    ) -> Result<BuildDetail> {
        self.post_json(
            "/builds",
            &BuildRequest {
                spec_ids,
                base_branch,
                rationale: None,
                evidence: None,
            },
        )
    }

    // --- mode ---

    pub fn mode(&self) -> Result<Mode> {
        let response: ModeResponse = self.get_json("/mode", &[])?;
        Ok(response.mode)
    }

    /// Mode gates *new* work only — pausing never interrupts a running scout.
    pub fn set_mode(&self, mode: Mode) -> Result<Mode> {
        let response: ModeResponse = self.post_json(
            "/mode",
            &SetMode {
                mode: mode.as_str().to_string(),
            },
        )?;
        Ok(response.mode)
    }

    // --- briefings ---

    /// Stale-while-revalidate: this read *is* the regeneration demand signal.
    /// Refetch on `briefing_updated` events; never poll on a timer.
    pub fn briefings(&self) -> Result<Vec<BriefingStatus>> {
        self.get_json("/briefings", &[])
    }

    // --- events ---

    /// Catch-up read of the event log. With `since` (inclusive), forward from
    /// that seq; without it, the newest `limit` (server default 100).
    pub fn events(&self, since: Option<i64>, limit: Option<i64>) -> Result<Vec<Event>> {
        let mut query = Vec::new();
        if let Some(since) = since {
            query.push(("since", since.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        self.get_json("/events", &query)
    }

    // --- orchestrator ---

    /// Turns after `since`, oldest first — the incremental catch-up a client
    /// should use once it has opened the conversation. The server caps the
    /// page, so a client far behind calls this until it comes back short.
    pub fn orchestrator_messages(&self, since: i64) -> Result<Vec<OrchestratorMessage>> {
        self.get_json("/orchestrator/messages", &[("since", since.to_string())])
    }

    /// The newest `limit` turns — how a client opens the conversation
    /// without dragging the whole history across.
    pub fn orchestrator_messages_latest(&self, limit: i64) -> Result<Vec<OrchestratorMessage>> {
        self.get_json("/orchestrator/messages", &[("limit", limit.to_string())])
    }

    /// The `limit` turns immediately before `before`, oldest first — paging
    /// backwards through history that is kept but not held in memory.
    pub fn orchestrator_messages_before(
        &self,
        before: i64,
        limit: i64,
    ) -> Result<Vec<OrchestratorMessage>> {
        self.get_json(
            "/orchestrator/messages",
            &[("before", before.to_string()), ("limit", limit.to_string())],
        )
    }

    /// 202: the reply arrives asynchronously — watch `orchestrator_message`
    /// events (or [`Client::stream_orchestrator`] for the live feed).
    pub fn send_orchestrator_message(
        &self,
        content: impl Into<String>,
    ) -> Result<OrchestratorMessage> {
        self.post_json(
            "/orchestrator/messages",
            &SendMessage {
                content: content.into(),
            },
        )
    }

    pub fn orchestrator_session(&self) -> Result<OrchestratorSessionInfo> {
        self.get_json("/orchestrator/session", &[])
    }

    /// Claim the interactive checkout. Re-POST at least every 5 minutes to
    /// keep the heartbeat fresh; 409 when no CC session exists yet.
    pub fn checkout_orchestrator_session(&self) -> Result<OrchestratorSessionInfo> {
        self.post_empty("/orchestrator/session/checkout")
    }

    /// Idempotent.
    pub fn release_orchestrator_session(&self) -> Result<OrchestratorSessionInfo> {
        self.post_empty("/orchestrator/session/release")
    }

    // --- streams ---

    /// The invalidation feed. The iterator connects on first `next()`,
    /// reconnects with a delay after drops, and never ends (unless the
    /// server answers with an HTTP error — that's terminal). The contract:
    /// on every [`EventStreamItem::Connected`], snapshot the lists you care
    /// about — it fires *before* any event from the new connection, so
    /// nothing that happened while disconnected can slip between.
    pub fn stream_events(&self) -> EventStream {
        EventStream::new(self.clone())
    }

    /// Live tail of a session's transcript, gapless: the server replays from
    /// `since` (inclusive) before going live, and reconnects resume from the
    /// last delivered seq. Ends only on a terminal (HTTP) error — the caller
    /// decides when a completed session's tail is no longer worth holding.
    pub fn stream_transcript(&self, id: &SessionId, since: i64) -> TranscriptTail {
        TranscriptTail::new(self.clone(), id.clone(), since)
    }

    /// The in-flight orchestrator tick (deltas, tool labels, done).
    /// Ephemeral: no backfill exists, so after a drop just resync the
    /// durable state via [`Client::orchestrator_messages`].
    pub fn stream_orchestrator(&self) -> OrchestratorFeed {
        OrchestratorFeed::new(self.clone())
    }
}
