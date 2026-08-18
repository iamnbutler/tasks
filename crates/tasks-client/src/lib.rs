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
mod version;

pub use sse::{EventStream, EventStreamItem, OrchestratorFeed, TranscriptTail};
pub use tasks_api as api;
pub use version::{CLIENT_COMMIT, CLIENT_VERSION, Preflight};

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tasks_api::events::Event;
use tasks_api::http::{
    BuildDetail, BuildNowRequest, BuildRequest, CancelAck, CancelAllResponse, CancelRunRequest,
    CaptureIssue, CloseTaskRequest, CreateProject, ErrorResponse, ModeResponse, RejectedBundle,
    ReopenTaskRequest, ReorderQueue, ReorderSpecQueue, ReviewRequest, ScoutRequest, SendMessage,
    ServerStatus, SetCharter, SetMode, SetProjectStatus,
};
use tasks_api::models::{
    Build, BuildId, Capability, CharterEntry, CharterLevel, CloseReason, Mode, OrchestratorMessage,
    OrchestratorSessionInfo, Project, ProjectId, ProjectStatus, ScoutNotes, Session, SessionId,
    Spec, SpecId, SpecQueueItem, SpecQueueStatus, Task, TaskId, TranscriptLine, TranscriptOwner,
};
use tasks_api::version::VersionInfo;
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
    /// The build [`Client::preflight`] reports as "this client". Defaults to
    /// this crate's own stamp; an app overrides it with the version it shows
    /// in About, so the warning names a number the user can see.
    client_version: String,
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
            client_version: CLIENT_VERSION.to_string(),
        }
    }

    /// Report `version` as this client's build in [`Client::preflight`].
    ///
    /// Pass whatever the UI shows the user (the app passes its About
    /// version): a warning that names a number nobody can find on screen is
    /// most of the way to no warning at all.
    pub fn with_client_version(mut self, version: impl Into<String>) -> Self {
        self.client_version = version.into();
        self
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

    /// A delete whose only answer is 204 — no body to decode.
    fn delete(&self, path: &str) -> Result<()> {
        self.calls
            .delete(&self.url(path))
            .call()
            .map_err(map_ureq)?;
        Ok(())
    }

    // --- version ---

    /// The server's build identity. Cheap and store-free at the other end, so
    /// this also answers while the server is still starting up.
    pub fn server_version(&self) -> Result<VersionInfo> {
        self.get_json("/version", &[])
    }

    /// Check this client's build against the server's floor. Call it on
    /// connect (and on every reconnect — a reconnect is usually a server that
    /// restarted into a new build) and put [`Preflight::warning`] in a banner.
    ///
    /// A 404 is a *verdict*, not an error: a server without `/version`
    /// predates the route, which makes it the stale end of the pair. Only a
    /// transport failure — nothing listening, connection reset — is `Err`,
    /// and that is the caller's existing "can't reach the server" case.
    pub fn preflight(&self) -> Result<Preflight> {
        match self.server_version() {
            Ok(server) => Ok(Preflight::judge(&self.client_version, server)),
            Err(ClientError::Api { status: 404, .. }) => Ok(Preflight::ServerUnversioned {
                client: self.client_version.clone(),
            }),
            Err(err) => Err(err),
        }
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

    /// How much of the pipeline runs for one repo — the per-repo counterpart
    /// to [`Client::set_mode`], and the only removal there is (archive, never
    /// delete). Gates *new* work only: a scout or build already in flight for
    /// this project runs to its own conclusion. Human-only at the server.
    pub fn set_project_status(
        &self,
        project_id: &ProjectId,
        status: ProjectStatus,
    ) -> Result<Project> {
        self.post_json(
            &format!("/projects/{project_id}/status"),
            &SetProjectStatus {
                status: status.as_str().to_string(),
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
    ///
    /// `directions` aims the run — what to look at, a constraint the issue
    /// does not state — and is staged on the task until a Scout takes it.
    /// `None` leaves whatever is already staged alone; `Some("")` clears it.
    /// It is not a rationale: this text is the only thing here the agent
    /// actually reads.
    pub fn scout_task_now(&self, id: &TaskId, directions: Option<String>) -> Result<Task> {
        self.post_json(
            &format!("/tasks/{id}/scout"),
            &ScoutRequest {
                directions,
                ..Default::default()
            },
        )
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

    /// Reopen the GitHub issue behind a retired task — the recourse half of
    /// [`Client::close_task`], and 202 for the same reason: open-or-closed is
    /// GitHub's fact, and the poller reads it back on its next pass.
    ///
    /// Which is why a caller should not gate this on the task's *local*
    /// `gh_state`: it lags a close by up to a poll interval, and that is
    /// exactly the window in which someone wants the undo.
    pub fn reopen_task(&self, id: &TaskId, rationale: Option<String>) -> Result<()> {
        self.post_json_accepted(
            &format!("/tasks/{id}/reopen"),
            &ReopenTaskRequest {
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

    /// Salvage from a session that stopped early: what the scout had written
    /// down when it was interrupted.
    ///
    /// `None` rather than an error for the 404, because having no notes is
    /// the ordinary case — a session that concluded has none, and neither
    /// does one that left nothing behind. **Not a spec**: these notes were
    /// never reviewed and carry no verdict, so anything rendering them should
    /// say so.
    pub fn session_notes(&self, id: &SessionId) -> Result<Option<ScoutNotes>> {
        match self.get_json::<ScoutNotes>(&format!("/sessions/{id}/notes"), &[]) {
            Ok(notes) => Ok(Some(notes)),
            Err(ClientError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Catch-up read of a scout session's transcript. `since` is inclusive — a
    /// tailing caller passes `last_seq + 1`. Prefer
    /// [`Client::stream_transcript`] for live tails.
    pub fn transcript(
        &self,
        id: &SessionId,
        since: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<TranscriptLine>> {
        self.get_json(
            &format!("/sessions/{id}/transcript"),
            &transcript_query(since, limit),
        )
    }

    /// The same read for a build. `seq` restarts at 1 per owner, so a caller
    /// paging both a build and its specs' scout sessions needs one cursor per
    /// owner — never one per task.
    pub fn build_transcript(
        &self,
        id: &BuildId,
        since: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<TranscriptLine>> {
        self.get_json(
            &format!("/builds/{id}/transcript"),
            &transcript_query(since, limit),
        )
    }

    // --- stopping work in flight ---

    /// Stop a running scout.
    ///
    /// The run concludes as `cancelled` — never `failed` — and the task goes
    /// back to the **backlog** rather than the queue, so the dispatch loop does
    /// not immediately start a replacement. Re-queueing is
    /// [`Client::queue_task`], by the person who stopped it.
    ///
    /// `rationale` is what makes the stop legible afterwards: it lands in the
    /// session's `exit_reason`, which is otherwise indistinguishable from a
    /// crash. The server only *requires* one of the orchestrator, so a caller
    /// that means it to be mandatory enforces that itself.
    ///
    /// [`CancelAck::concluded`] is `false` when the run had not stopped yet by
    /// the time the server answered — recorded, not failed. Watch for the
    /// run's completion event.
    pub fn cancel_session(&self, id: &SessionId, rationale: Option<String>) -> Result<CancelAck> {
        self.post_json(
            &format!("/sessions/{id}/cancel"),
            &CancelRunRequest {
                rationale,
                evidence: None,
            },
        )
    }

    /// Cancel everything that currently holds a VM — every `running` session
    /// and `running` build — through the same per-run writes as the two
    /// single cancels, so each run's `exit_reason` carries the rationale
    /// individually. Queued builds survive: they hold no container, and
    /// killing containers must not rewrite the queue.
    pub fn cancel_all_runs(&self, rationale: Option<String>) -> Result<CancelAllResponse> {
        self.post_json(
            "/runs/cancel-all",
            &CancelRunRequest {
                rationale,
                evidence: None,
            },
        )
    }

    /// [`Client::cancel_session`] for a build, queued or running. Its specs go
    /// back to `approved` and their tasks to `ready_to_build`, with no build
    /// attempt charged — a cancelled build says nothing about whether the work
    /// can be built.
    pub fn cancel_build(&self, id: &BuildId, rationale: Option<String>) -> Result<CancelAck> {
        self.post_json(
            &format!("/builds/{id}/cancel"),
            &CancelRunRequest {
                rationale,
                evidence: None,
            },
        )
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
    /// `directions` reaches the Builder's prompt as its own section; it is
    /// deliberately not the `rationale`, which no VM ever sees.
    pub fn request_build(
        &self,
        spec_ids: Vec<SpecId>,
        base_branch: Option<String>,
        directions: Option<String>,
    ) -> Result<BuildDetail> {
        self.post_json(
            "/builds",
            &BuildRequest {
                spec_ids,
                base_branch,
                rationale: None,
                directions,
                evidence: None,
            },
        )
    }

    // --- preserved bundles ---

    /// Every implementation whose branch could not be pushed, newest first.
    ///
    /// An empty list means the server looked and found nothing. A server with
    /// no bundle directory configured answers 503 instead — deliberately not
    /// an empty list, since "nothing was preserved" is the one wrong thing to
    /// say about work that exists in exactly one place.
    pub fn bundles(&self) -> Result<Vec<RejectedBundle>> {
        self.get_json("/bundles", &[])
    }

    /// One build's preserved bundle. `None` rather than an error for the 404,
    /// like [`Self::session_notes`]: every build that landed its branch has
    /// none, which is the overwhelming majority.
    pub fn build_bundle(&self, id: &BuildId) -> Result<Option<RejectedBundle>> {
        match self.get_json::<RejectedBundle>(&format!("/builds/{id}/bundle"), &[]) {
            Ok(bundle) => Ok(Some(bundle)),
            Err(ClientError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Throw a preserved implementation away. There is no undo, and the
    /// retention policy already reclaims anything that has been rebuilt and
    /// shipped — so what this deletes is, by construction, the only copy of
    /// something nobody reproduced.
    ///
    /// Human-only at the server. A 404 means it was already gone (a second
    /// click, or a reclaim that got there first).
    pub fn delete_build_bundle(&self, id: &BuildId) -> Result<()> {
        self.delete(&format!("/builds/{id}/bundle"))
    }

    /// "Build now": write the spec by hand, approve it, and queue the Builder
    /// run — one call, because from the human's side it is one decision.
    ///
    /// For a task whose issue body already *is* the specification. The spec is
    /// always that body: `content` is an API-level override this client does
    /// not offer, because if the body is not the spec the honest answers are to
    /// scout the task or edit the issue, not to type a description only Tasks
    /// can see.
    ///
    /// `rationale` is what makes an unreviewed build reviewable afterwards —
    /// nothing else in this path carries a second opinion. The server does not
    /// demand it (only the orchestrator is ever gated on one, and the
    /// orchestrator is refused this endpoint outright), so callers that mean it
    /// to be mandatory enforce that themselves.
    ///
    /// 202, like [`Self::request_build`]: queued, not started.
    /// `directions` is kept out of the spec this authors: the spec is the
    /// issue body, and an instruction to the agent does not belong in the
    /// artifact.
    pub fn build_task_now(
        &self,
        id: &TaskId,
        rationale: Option<String>,
        directions: Option<String>,
    ) -> Result<BuildDetail> {
        self.post_json(
            &format!("/tasks/{id}/build-now"),
            &BuildNowRequest {
                rationale,
                directions,
                ..Default::default()
            },
        )
    }

    // --- status ---

    /// Who is serving, since when, what that boot migrated, and what is in
    /// flight. A successful call is the claim that *this* pid opened the
    /// database and finished its migrations — which is why it doubles as the
    /// liveness probe for a swap.
    pub fn status(&self) -> Result<ServerStatus> {
        self.get_json("/status", &[])
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
        TranscriptTail::new(self.clone(), TranscriptOwner::session(id), since)
    }

    /// The same tail for a build.
    pub fn stream_build_transcript(&self, id: &BuildId, since: i64) -> TranscriptTail {
        TranscriptTail::new(self.clone(), TranscriptOwner::build(id), since)
    }

    /// The in-flight orchestrator tick (deltas, tool labels, done).
    /// Ephemeral: no backfill exists, so after a drop just resync the
    /// durable state via [`Client::orchestrator_messages`].
    pub fn stream_orchestrator(&self) -> OrchestratorFeed {
        OrchestratorFeed::new(self.clone())
    }
}

/// `?since=&limit=` for the two transcript reads — omitted entirely when the
/// caller passed nothing, so the server's defaults apply.
fn transcript_query(since: Option<i64>, limit: Option<i64>) -> Vec<(&'static str, String)> {
    let mut query = Vec::new();
    if let Some(since) = since {
        query.push(("since", since.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    query
}
