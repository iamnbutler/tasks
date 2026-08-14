//! Headless HTTP control API.
//!
//! This is the first (and for now only) interface onto the store: Claude Code,
//! the CLI and any later UI all drive Tasks through these routes. Reads are
//! plain JSON projections of the store; writes append events so anything
//! watching `/events/stream` sees the whole picture.
//!
//! Binds to loopback only — there is no authentication.

use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use serde::Deserialize;
use thiserror::Error;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tracing::{error, info, warn};

use tasks_api::http::{
    BriefingStatus, BuildDetail, BuildRequest, CreateProject, ErrorResponse, ModeResponse,
    ReorderQueue, ReorderSpecQueue, ReviewRequest, SendMessage, SetMode,
};

use crate::briefing::{self, Briefings};
use crate::events::{Event, EventPayload};
use crate::models::{
    Actor, Build, BuildId, ChatRole, Decision, DecisionInput, Mode, OrchestratorMessage,
    OrchestratorSessionInfo, Project, ProjectId, Session, SessionId, Spec, SpecId, SpecQueueItem,
    SpecQueueStatus, Task, TaskId, TranscriptLine,
};
use crate::store::{Store, StoreError};

/// How many events `/events` returns when the caller doesn't ask for a count.
const DEFAULT_EVENT_LIMIT: i64 = 100;

/// Interval between SSE keep-alive comments.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Transcript lines `/sessions/{id}/transcript` returns without an explicit
/// `limit`, and the ceiling on one the caller asks for. The SSE replay pages at
/// the maximum until it catches up.
const DEFAULT_TRANSCRIPT_LIMIT: i64 = 500;
const MAX_TRANSCRIPT_LIMIT: i64 = 2000;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Internal(String),
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound(what) => ApiError::NotFound(what),
            StoreError::Invalid(what) => ApiError::BadRequest(what),
            StoreError::Sqlx(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                ApiError::BadRequest(format!("already exists: {}", db.message()))
            }
            other => {
                error!(error = %other, "store error");
                ApiError::Internal(other.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ErrorResponse {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Router state: the store plus the optional briefing service. Optional so
/// `router(store)` (tests, embedded uses) keeps working — without the
/// service, `GET /briefings` serves stored copies and never regenerates.
#[derive(Clone)]
pub struct AppState {
    store: Arc<Store>,
    briefings: Option<Arc<Briefings>>,
}

impl FromRef<AppState> for Arc<Store> {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

impl FromRef<AppState> for Option<Arc<Briefings>> {
    fn from_ref(state: &AppState) -> Self {
        state.briefings.clone()
    }
}

/// Build the API router over a store, with no briefing service. Exposed
/// separately from [`serve`] so tests can bind their own listener.
pub fn router(store: Arc<Store>) -> Router {
    router_with_briefings(store, None)
}

/// Build the full API router. `serve` passes the briefing service so
/// `GET /briefings` can kick stale-while-revalidate regenerations.
pub fn router_with_briefings(store: Arc<Store>, briefings: Option<Arc<Briefings>>) -> Router {
    Router::new()
        .route("/projects", get(list_projects).post(create_project))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{task_id}", get(get_task))
        .route("/tasks/{task_id}/queue", post(queue_task))
        .route("/tasks/{task_id}/dequeue", post(dequeue_task))
        .route("/tasks/{task_id}/scout", post(scout_task_now))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{session_id}", get(get_session))
        .route("/sessions/{session_id}/transcript", get(list_transcript))
        .route(
            "/sessions/{session_id}/transcript/stream",
            get(stream_transcript),
        )
        .route("/specs", get(list_specs))
        .route("/specs/{spec_id}", get(get_spec))
        .route("/spec-queue", get(list_spec_queue))
        .route("/spec-queue/reorder", post(reorder_spec_queue))
        .route("/spec-queue/{spec_id}/review", post(review_spec))
        .route("/builds", get(list_builds).post(request_build))
        .route("/decisions", get(list_decisions))
        .route("/builds/{build_id}", get(get_build))
        .route(
            "/orchestrator/messages",
            get(list_orchestrator_messages).post(send_orchestrator_message),
        )
        .route("/orchestrator/stream", get(stream_orchestrator))
        .route("/orchestrator/session", get(get_orchestrator_session))
        .route(
            "/orchestrator/session/checkout",
            post(checkout_orchestrator_session),
        )
        .route(
            "/orchestrator/session/release",
            post(release_orchestrator_session),
        )
        .route("/mode", get(get_mode).post(set_mode))
        .route("/queue/reorder", post(reorder_queue))
        .route("/briefings", get(list_briefings))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .with_state(AppState { store, briefings })
}

/// Serve the API on loopback at `port`. Runs until the process is killed.
pub async fn serve(store: Arc<Store>, port: u16) -> std::io::Result<()> {
    serve_with_shutdown(store, None, port, std::future::pending()).await
}

/// Serve the API on loopback at `port` until `shutdown` resolves, then stop
/// accepting connections and let the in-flight ones drain.
pub async fn serve_with_shutdown(
    store: Arc<Store>,
    briefings: Option<Arc<Briefings>>,
    port: u16,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    info!(addr = %listener.local_addr()?, "tasks api listening");
    axum::serve(listener, router_with_briefings(store, briefings))
        .with_graceful_shutdown(shutdown)
        .await
}

// --- projects ---

async fn list_projects(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(store.list_projects().await?))
}

async fn create_project(
    State(store): State<Arc<Store>>,
    Json(body): Json<CreateProject>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    if body.repo_owner.is_empty() || body.repo_name.is_empty() {
        return Err(ApiError::BadRequest(
            "repo_owner and repo_name must be non-empty".into(),
        ));
    }
    let project = Project {
        id: ProjectId::new(),
        repo_owner: body.repo_owner,
        repo_name: body.repo_name,
        added_at: Utc::now(),
    };
    store.insert_project(&project).await?;
    store
        .append_event(EventPayload::ProjectAdded {
            project_id: project.id.clone(),
        })
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

// --- tasks ---

#[derive(Debug, Deserialize)]
struct TasksQuery {
    /// `?all=true` returns every row. The default hides tasks whose issue is
    /// closed on GitHub and that never left `new` — intake noise from issues
    /// that died before any work started.
    all: Option<bool>,
}

async fn list_tasks(
    State(store): State<Arc<Store>>,
    Query(query): Query<TasksQuery>,
) -> ApiResult<Json<Vec<Task>>> {
    let tasks = match query.all.unwrap_or(false) {
        true => store.list_tasks().await?,
        false => store.list_active_tasks().await?,
    };
    Ok(Json(tasks))
}

async fn get_task(
    State(store): State<Arc<Store>>,
    Path(task_id): Path<String>,
) -> ApiResult<Json<Task>> {
    let id = TaskId::from_raw(task_id);
    store
        .get_task(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("task {id}")))
}

/// Pick a backlog task up into the scout queue (appended at the end).
async fn queue_task(
    State(store): State<Arc<Store>>,
    Path(task_id): Path<String>,
) -> ApiResult<Json<Task>> {
    Ok(Json(store.queue_task(&TaskId::from_raw(task_id)).await?))
}

/// Return a queued (not yet running) task to the backlog.
async fn dequeue_task(
    State(store): State<Arc<Store>>,
    Path(task_id): Path<String>,
) -> ApiResult<Json<Task>> {
    Ok(Json(store.dequeue_task(&TaskId::from_raw(task_id)).await?))
}

/// "Scout now": queue the task at the front. The dispatch loop picks it up on
/// its next tick; the concurrency cap still applies.
async fn scout_task_now(
    State(store): State<Arc<Store>>,
    Path(task_id): Path<String>,
) -> ApiResult<Json<Task>> {
    Ok(Json(
        store.push_task_to_front(&TaskId::from_raw(task_id)).await?,
    ))
}

/// Returns the same projection as the default `GET /tasks` — a client applies
/// the response in place of its list, so handing back the unfiltered variant
/// here would resurrect the closed-intake rows the default read hides.
async fn reorder_queue(
    State(store): State<Arc<Store>>,
    Json(body): Json<ReorderQueue>,
) -> ApiResult<Json<Vec<Task>>> {
    store.set_queue_order(&body.task_ids).await?;
    Ok(Json(store.list_active_tasks().await?))
}

// --- sessions ---

async fn list_sessions(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Session>>> {
    Ok(Json(store.list_sessions().await?))
}

async fn get_session(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Session>> {
    let id = SessionId::from_raw(session_id);
    store
        .get_session(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("session {id}")))
}

// --- specs ---

async fn list_specs(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Spec>>> {
    Ok(Json(store.list_specs().await?))
}

async fn get_spec(
    State(store): State<Arc<Store>>,
    Path(spec_id): Path<String>,
) -> ApiResult<Json<Spec>> {
    let id = SpecId::from_raw(spec_id);
    store
        .get_spec(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("spec {id}")))
}

async fn list_spec_queue(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<SpecQueueItem>>> {
    Ok(Json(store.list_spec_queue().await?))
}

/// Header the orchestrator identifies itself with: `orchestrator <token>`,
/// where the token was minted for it at boot and handed over in its
/// environment. Everything else is the human.
const ACTOR_HEADER: &str = "x-tasks-actor";

fn actor_of(store: &Store, headers: &axum::http::HeaderMap) -> Actor {
    store.resolve_actor(headers.get(ACTOR_HEADER).and_then(|v| v.to_str().ok()))
}

/// The decisions ledger. `?spec=` / `?build=` narrow to one subject; the
/// default is the whole ledger, newest first.
async fn list_decisions(
    State(store): State<Arc<Store>>,
    Query(q): Query<DecisionQuery>,
) -> ApiResult<Json<Vec<Decision>>> {
    let subject = match (q.spec.as_deref(), q.build.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::BadRequest("pass spec or build, not both".into()));
        }
        (Some(id), None) => Some(("spec", id)),
        (None, Some(id)) => Some(("build", id)),
        (None, None) => None,
    };
    Ok(Json(
        store.decisions(subject, q.limit.unwrap_or(100)).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct DecisionQuery {
    spec: Option<String>,
    build: Option<String>,
    limit: Option<i64>,
}

async fn reorder_spec_queue(
    State(store): State<Arc<Store>>,
    Json(body): Json<ReorderSpecQueue>,
) -> ApiResult<Json<Vec<SpecQueueItem>>> {
    store.set_spec_queue_order(&body.spec_ids).await?;
    Ok(Json(store.list_spec_queue().await?))
}

async fn review_spec(
    State(store): State<Arc<Store>>,
    Path(spec_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ReviewRequest>,
) -> ApiResult<Json<SpecQueueItem>> {
    let status = SpecQueueStatus::from_str(&body.status)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown status: {}", body.status)))?;
    let id = SpecId::from_raw(spec_id);
    let decision = DecisionInput {
        actor: actor_of(&store, &headers),
        rationale: body.rationale,
        evidence: body.evidence,
    };
    let entry = store
        .review_spec(&id, status, body.feedback, decision)
        .await?;
    let spec = store
        .get_spec(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("spec {id}")))?;
    Ok(Json(SpecQueueItem {
        entry,
        task_id: spec.task_id,
    }))
}

// --- builds ---

/// 202, not 200/201-with-result: builds are serial and this only queues one.
/// Watch `build_started` / `build_completed` on the event stream, or poll
/// `GET /builds/{id}`.
async fn request_build(
    State(store): State<Arc<Store>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<BuildRequest>,
) -> ApiResult<(StatusCode, Json<BuildDetail>)> {
    let base_branch = body.base_branch.as_deref().unwrap_or("main");
    let decision = DecisionInput {
        actor: actor_of(&store, &headers),
        rationale: body.rationale,
        evidence: body.evidence,
    };
    let build = store
        .create_build(&body.spec_ids, base_branch, decision)
        .await?;
    let spec_ids = store.build_spec_ids(&build.id).await?;
    Ok((StatusCode::ACCEPTED, Json(BuildDetail { build, spec_ids })))
}

async fn list_builds(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Build>>> {
    Ok(Json(store.list_builds().await?))
}

async fn get_build(
    State(store): State<Arc<Store>>,
    Path(build_id): Path<String>,
) -> ApiResult<Json<BuildDetail>> {
    let id = BuildId::from_raw(build_id);
    let build = store
        .get_build(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("build {id}")))?;
    let spec_ids = store.build_spec_ids(&id).await?;
    Ok(Json(BuildDetail { build, spec_ids }))
}

// --- orchestrator ---

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    #[serde(default)]
    since: i64,
}

/// 202: the message is queued; the orchestrator loop answers it. Watch
/// `orchestrator_message` events (or poll) for the reply.
async fn send_orchestrator_message(
    State(store): State<Arc<Store>>,
    Json(body): Json<SendMessage>,
) -> ApiResult<(StatusCode, Json<OrchestratorMessage>)> {
    let content = body.content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest("message is empty".into()));
    }
    let message = store
        .append_orchestrator_message(ChatRole::User, content)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(message)))
}

async fn list_orchestrator_messages(
    State(store): State<Arc<Store>>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult<Json<Vec<OrchestratorMessage>>> {
    Ok(Json(store.orchestrator_messages_since(q.since).await?))
}

/// The orchestrator's CC session, for interactive resume
/// (`cd <workdir> && claude --resume <cc_session_id>`).
async fn get_orchestrator_session(
    State(store): State<Arc<Store>>,
) -> ApiResult<Json<OrchestratorSessionInfo>> {
    Ok(Json(store.orchestrator_session_info().await?))
}

/// Renew the interactive-checkout heartbeat. While it's fresh, headless
/// ticks are suspended (CC sessions have no file locking); nudges still
/// accumulate as unanswered turns and are answered after release. Callers
/// re-POST at least once per [`crate::store::ORCHESTRATOR_CHECKOUT_TTL`].
/// 409 when no CC session exists yet — there is nothing to check out.
async fn checkout_orchestrator_session(
    State(store): State<Arc<Store>>,
) -> ApiResult<Json<OrchestratorSessionInfo>> {
    let info = store.orchestrator_session_info().await?;
    if info.cc_session_id.is_none() {
        return Err(ApiError::Conflict(
            "no orchestrator session exists yet".into(),
        ));
    }
    store.orchestrator_checkout().await?;
    Ok(Json(store.orchestrator_session_info().await?))
}

/// End the interactive checkout; the next tick may resume the session.
/// Idempotent — releasing an unclaimed session is fine.
async fn release_orchestrator_session(
    State(store): State<Arc<Store>>,
) -> ApiResult<Json<OrchestratorSessionInfo>> {
    store.orchestrator_release().await?;
    Ok(Json(store.orchestrator_session_info().await?))
}

/// SSE feed of the in-flight orchestrator tick: `delta` chunks as the reply
/// is generated, `tool` labels as the agent works, `done` when the durable
/// message has landed in `/orchestrator/messages`. Ephemeral — there is no
/// backfill, and a lagged or reconnecting client just resyncs by fetching
/// the messages; nothing here is ever the source of truth.
async fn stream_orchestrator(
    State(store): State<Arc<Store>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(store.subscribe_orchestrator_feed()).filter_map(|result| {
        let event = match result {
            Ok(event) => event,
            Err(err) => {
                warn!(error = %err, "orchestrator feed subscriber lagged");
                return None;
            }
        };
        match SseEvent::default().json_data(&event) {
            Ok(sse) => Some(Ok(sse)),
            Err(err) => {
                error!(error = %err, "serializing orchestrator feed event for sse");
                None
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE))
}

// --- mode ---

async fn get_mode(State(store): State<Arc<Store>>) -> ApiResult<Json<ModeResponse>> {
    Ok(Json(ModeResponse {
        mode: store.get_mode().await?,
    }))
}

async fn set_mode(
    State(store): State<Arc<Store>>,
    Json(body): Json<SetMode>,
) -> ApiResult<Json<ModeResponse>> {
    let mode = Mode::from_str(&body.mode)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown mode: {}", body.mode)))?;
    let from = store.get_mode().await?;
    store.set_mode(mode).await?;
    if from != mode {
        store
            .append_event(EventPayload::ModeChanged { from, to: mode })
            .await?;
    }
    Ok(Json(ModeResponse { mode }))
}

// --- transcripts ---

#[derive(Debug, Deserialize)]
struct TranscriptQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

/// Catch-up read of a session's agent output. `since` is **inclusive**, to
/// match `/events?since=` — a tailing client passes `last_seq + 1`. An empty
/// array means "nothing recorded", not an error: sessions that predate
/// transcript capture have none.
async fn list_transcript(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> ApiResult<Json<Vec<TranscriptLine>>> {
    let session_id = SessionId::from_raw(session_id);
    if store.get_session(&session_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("session {session_id}")));
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TRANSCRIPT_LIMIT)
        .min(MAX_TRANSCRIPT_LIMIT);
    if limit <= 0 {
        return Err(ApiError::BadRequest("limit must be positive".into()));
    }
    let lines = store
        .transcript_since(&session_id, query.since.unwrap_or(0), limit)
        .await?;
    Ok(Json(lines))
}

/// SSE tail of a session's transcript: replay from `since`, then live lines.
///
/// The replay pages until it catches up and deliberately takes no `limit`. The
/// obvious alternative — one limit-sized page, then attach the tail — hands the
/// client a stream that jumps silently from the end of the page to the newest
/// line, and a stream is exactly the shape in which that hole goes unnoticed.
/// The per-session byte cap is what keeps reading it all affordable.
async fn stream_transcript(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let session_id = SessionId::from_raw(session_id);
    if store.get_session(&session_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("session {session_id}")));
    }

    // Subscribe *before* reading history, so a line appended between the two
    // steps arrives on the live channel instead of falling down the gap.
    let live = store.subscribe_transcript();

    let mut backfill = Vec::new();
    let mut next = query.since.unwrap_or(0);
    loop {
        let page = store
            .transcript_since(&session_id, next, MAX_TRANSCRIPT_LIMIT)
            .await?;
        let Some(last) = page.last() else { break };
        next = last.seq + 1;
        let full = page.len() as i64 == MAX_TRANSCRIPT_LIMIT;
        backfill.extend(page);
        if !full {
            break;
        }
    }

    let session_filter = session_id.clone();
    let tail = BroadcastStream::new(live).filter_map(move |result| {
        let line = match result {
            Ok(line) => line,
            Err(err) => {
                warn!(error = %err, "transcript sse subscriber lagged");
                return None;
            }
        };
        // Drop other sessions, and anything the backfill already delivered.
        if line.session_id != session_filter || line.seq < next {
            return None;
        }
        to_sse(&line)
    });

    let replay: Vec<_> = backfill.iter().filter_map(to_sse).collect();
    let stream = tokio_stream::iter(replay).chain(tail);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE)))
}

fn to_sse(line: &TranscriptLine) -> Option<Result<SseEvent, Infallible>> {
    match SseEvent::default().json_data(line) {
        Ok(sse) => Some(Ok(sse)),
        Err(err) => {
            error!(error = %err, seq = line.seq, "serializing transcript line for sse");
            None
        }
    }
}

// --- briefings ---

/// All three Home briefing slots, stale-while-revalidate: whatever is stored
/// returns immediately, and stale sections kick a single-flight background
/// regeneration when a briefing service is attached (the production server).
/// Completion arrives as a `briefing_updated` event — refetch on it. Without
/// a service (tests, embedded routers) this only ever serves stored copies.
async fn list_briefings(
    State(store): State<Arc<Store>>,
    State(briefings): State<Option<Arc<Briefings>>>,
) -> ApiResult<Json<Vec<BriefingStatus>>> {
    match briefings {
        Some(service) => Ok(Json(service.get_all().await?)),
        None => Ok(Json(
            briefing::snapshot(&store, briefing::DEFAULT_TTL).await?,
        )),
    }
}

// --- events ---

#[derive(Debug, Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

/// Catch-up read of the event log. With `since`, returns up to `limit` events
/// from that seq forward; without it, the most recent `limit` events.
async fn list_events(
    State(store): State<Arc<Store>>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<Vec<Event>>> {
    let limit = query.limit.unwrap_or(DEFAULT_EVENT_LIMIT);
    if limit <= 0 {
        return Err(ApiError::BadRequest("limit must be positive".into()));
    }
    let events = match query.since {
        Some(since) => store.events_since(since, limit).await?,
        None => store.recent_events(limit).await?,
    };
    Ok(Json(events))
}

async fn stream_events(
    State(store): State<Arc<Store>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(store.subscribe_events()).filter_map(|result| {
        let event = match result {
            Ok(event) => event,
            // A subscriber that falls behind the broadcast buffer must resync
            // through GET /events?since=; we just keep the stream alive.
            Err(err) => {
                warn!(error = %err, "sse subscriber lagged");
                return None;
            }
        };
        match SseEvent::default().json_data(&event) {
            Ok(sse) => Some(Ok(sse)),
            Err(err) => {
                error!(error = %err, seq = event.seq, "serializing event for sse");
                None
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use super::*;
    use crate::models::{
        Complexity, GhState, Session, SessionStatus, Spec, SpecQueueEntry, TaskState,
    };

    /// Bind an OS-assigned port, serve the real router over a real store, and
    /// return the base URL.
    async fn spawn(store: Arc<Store>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(store)).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn store_with_project() -> (Arc<Store>, Project) {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let project = Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "tasks".into(),
            added_at: Utc::now(),
        };
        store.insert_project(&project).await.unwrap();
        (store, project)
    }

    async fn insert_task(store: &Store, project: &Project, number: u64, priority: i32) -> Task {
        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: number,
            title: format!("task {number}"),
            body: "body".into(),
            labels: vec![],
            gh_state: GhState::Open,
            state: TaskState::Backlog,
            priority,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
        };
        store.insert_task(&task).await.unwrap();
        task
    }

    /// Task + session + spec + pending-review queue entry.
    async fn insert_spec(store: &Store, project: &Project, number: u64) -> (Task, Spec) {
        let task = insert_task(store, project, number, 0).await;
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: format!("scout/{number}"),
            status: SessionStatus::ScoutSucceeded,
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();
        let spec = Spec {
            id: SpecId::new(),
            session_id: session.id.clone(),
            task_id: task.id.clone(),
            content: "## Spec".into(),
            complexity: Complexity::Simple,
            files_touched: vec![],
            created_at: Utc::now(),
        };
        store.insert_spec(&spec).await.unwrap();
        store
            .upsert_spec_queue_entry(&SpecQueueEntry {
                spec_id: spec.id.clone(),
                status: SpecQueueStatus::PendingReview,
                rank: None,
                approved_at: None,
                feedback: None,
                blocking_dependencies: vec![],
            })
            .await
            .unwrap();
        (task, spec)
    }

    #[tokio::test]
    async fn create_and_list_projects() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/projects"))
            .json(&json!({"repo_owner": "iamnbutler", "repo_name": "tasks"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let created: Project = resp.json().await.unwrap();
        assert_eq!(created.repo_name, "tasks");

        let listed: Vec<Project> = http
            .get(format!("{base}/projects"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed, vec![created.clone()]);

        // Same repo twice is a client error, not a 500
        let resp = http
            .post(format!("{base}/projects"))
            .json(&json!({"repo_owner": "iamnbutler", "repo_name": "tasks"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let events = store.all_events().await.unwrap();
        assert_eq!(
            events[0].payload,
            EventPayload::ProjectAdded {
                project_id: created.id
            }
        );
    }

    #[tokio::test]
    async fn queue_membership_over_http() {
        let (store, project) = store_with_project().await;
        let a = insert_task(&store, &project, 1, 0).await;
        let b = insert_task(&store, &project, 2, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        // Pick up a, then b: ranks append in order.
        let a1: Task = http
            .post(format!("{base}/tasks/{}/queue", a.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(a1.state, TaskState::Queued);
        assert_eq!(a1.manual_rank, Some(1));
        let b1: Task = http
            .post(format!("{base}/tasks/{}/queue", b.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(b1.manual_rank, Some(2));

        // Queueing a non-backlog task is a 400.
        let resp = http
            .post(format!("{base}/tasks/{}/queue", a.id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Scout-now on a backlog task queues it at the front, shifting others.
        let c = insert_task(&store, &project, 3, 0).await;
        let c1: Task = http
            .post(format!("{base}/tasks/{}/scout", c.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(c1.state, TaskState::Queued);
        assert_eq!(c1.manual_rank, Some(1));
        let order: Vec<TaskId> = store
            .list_tasks()
            .await
            .unwrap()
            .into_iter()
            .filter(|t| t.manual_rank.is_some())
            .map(|t| t.id)
            .collect();
        assert_eq!(order, vec![c.id.clone(), a.id.clone(), b.id.clone()]);

        // Dequeue returns to backlog and clears the rank.
        let b2: Task = http
            .post(format!("{base}/tasks/{}/dequeue", b.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(b2.state, TaskState::Backlog);
        assert_eq!(b2.manual_rank, None);

        // Dequeuing work that's past Queued is a 400; unknown ids are 404.
        store
            .update_task_state(&c.id, TaskState::Scouting)
            .await
            .unwrap();
        let resp = http
            .post(format!("{base}/tasks/{}/dequeue", c.id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = http
            .post(format!("{base}/tasks/nonexistent/queue"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // Scout-now on an already-queued task re-fronts it without a
        // duplicate state-change event.
        let a2: Task = http
            .post(format!("{base}/tasks/{}/scout", a.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(a2.manual_rank, Some(1));
        let queue_events = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| {
                matches!(&e.payload, EventPayload::TaskStateChanged { task_id, to: TaskState::Queued, .. } if *task_id == a.id)
            })
            .count();
        assert_eq!(queue_events, 1, "re-fronting must not re-announce pickup");
    }

    #[tokio::test]
    async fn queue_reorder_roundtrip_and_ordering() {
        let (store, project) = store_with_project().await;
        let high = insert_task(&store, &project, 1, 100).await;
        let low = insert_task(&store, &project, 2, 1).await;
        let unlisted = insert_task(&store, &project, 3, 50).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        // Priority order before any manual ranking
        let tasks: Vec<Task> = http
            .get(format!("{base}/tasks"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            vec![high.id.clone(), unlisted.id.clone(), low.id.clone()]
        );

        let resp = http
            .post(format!("{base}/queue/reorder"))
            .json(&json!({"task_ids": [low.id, high.id]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let tasks: Vec<Task> = resp.json().await.unwrap();

        // Manual rank beats priority; the unlisted task stays null and sorts last
        assert_eq!(
            tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            vec![low.id.clone(), high.id.clone(), unlisted.id.clone()]
        );
        assert_eq!(tasks[0].manual_rank, Some(1));
        assert_eq!(tasks[1].manual_rank, Some(2));
        assert_eq!(tasks[2].manual_rank, None);

        // A fresh GET sees the same order
        let tasks: Vec<Task> = http
            .get(format!("{base}/tasks"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(tasks[0].id, low.id);
    }

    /// An issue closed before any work started is intake noise; one that got as
    /// far as a spec is history a client still needs.
    #[tokio::test]
    async fn tasks_listing_hides_closed_intake_unless_all_is_asked_for() {
        let (store, project) = store_with_project().await;
        let open_new = insert_task(&store, &project, 1, 0).await;
        let closed_new = insert_task(&store, &project, 2, 0).await;
        let closed_spec_ready = insert_task(&store, &project, 3, 0).await;
        let retired = insert_task(&store, &project, 4, 0).await;
        let open_rejected = insert_task(&store, &project, 5, 0).await;
        store
            .reconcile_closed_issues(
                &project.id,
                &[open_new.gh_issue_number, open_rejected.gh_issue_number],
            )
            .await
            .unwrap();
        store
            .update_task_state(&closed_spec_ready.id, TaskState::InReview)
            .await
            .unwrap();
        store
            .update_task_state(&retired.id, TaskState::Done)
            .await
            .unwrap();
        store
            .update_task_state(&open_rejected.id, TaskState::Rejected)
            .await
            .unwrap();
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let visible: Vec<TaskId> = http
            .get(format!("{base}/tasks"))
            .send()
            .await
            .unwrap()
            .json::<Vec<Task>>()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(visible.contains(&open_new.id));
        assert!(
            visible.contains(&closed_spec_ready.id),
            "in-flight work stays visible even with the issue closed"
        );
        assert!(!visible.contains(&closed_new.id));
        assert!(
            !visible.contains(&retired.id),
            "concluded work behind a closed issue is history, not the working set"
        );
        assert!(
            visible.contains(&open_rejected.id),
            "a terminal task on a still-open issue is a pending decision, so it shows"
        );

        let all: Vec<TaskId> = http
            .get(format!("{base}/tasks?all=true"))
            .send()
            .await
            .unwrap()
            .json::<Vec<Task>>()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(all.len(), 5);
        assert!(all.contains(&closed_new.id));
        assert!(all.contains(&retired.id));

        // The reorder response is the same projection as the default read — a
        // client swaps it in for its list, so the unfiltered variant here
        // would resurrect the hidden closed-intake rows.
        let reordered: Vec<TaskId> = http
            .post(format!("{base}/queue/reorder"))
            .json(&serde_json::json!({ "task_ids": [open_new.id] }))
            .send()
            .await
            .unwrap()
            .json::<Vec<Task>>()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(reordered.contains(&open_new.id));
        assert!(reordered.contains(&closed_spec_ready.id));
        assert!(
            !reordered.contains(&closed_new.id),
            "reorder must not resurrect hidden closed tasks"
        );
    }

    #[tokio::test]
    async fn queue_reorder_unknown_task_is_404() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 1, 0).await;
        let base = spawn(store.clone()).await;

        let ghost = TaskId::new();
        let resp = reqwest::Client::new()
            .post(format!("{base}/queue/reorder"))
            .json(&json!({"task_ids": [task.id, ghost]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body: Value = resp.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains(ghost.as_str()));

        // Rejected wholesale — no partial ranking
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().manual_rank,
            None
        );
    }

    #[tokio::test]
    async fn spec_review_transitions() {
        let (store, project) = store_with_project().await;
        let (approved_task, approved_spec) = insert_spec(&store, &project, 1).await;
        let (revise_task, revise_spec) = insert_spec(&store, &project, 2).await;
        let (rejected_task, rejected_spec) = insert_spec(&store, &project, 3).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/spec-queue/{}/review", approved_spec.id))
            .json(&json!({"status": "approved", "feedback": "ship it"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let item: SpecQueueItem = resp.json().await.unwrap();
        assert_eq!(item.entry.status, SpecQueueStatus::Approved);
        assert!(item.entry.approved_at.is_some());
        assert_eq!(item.entry.feedback.as_deref(), Some("ship it"));
        assert_eq!(item.task_id, approved_task.id);
        assert_eq!(
            store
                .get_task(&approved_task.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            TaskState::ReadyToBuild
        );

        http.post(format!("{base}/spec-queue/{}/review", revise_spec.id))
            .json(&json!({"status": "needs_revision"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            store
                .get_task(&revise_task.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            TaskState::Queued
        );

        http.post(format!("{base}/spec-queue/{}/review", rejected_spec.id))
            .json(&json!({"status": "rejected", "feedback": "wrong shape"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            store
                .get_task(&rejected_task.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            TaskState::Rejected
        );
    }

    #[tokio::test]
    async fn spec_review_rejects_invalid_status_and_unknown_spec() {
        let (store, project) = store_with_project().await;
        let (_, spec) = insert_spec(&store, &project, 1).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        for status in ["bogus", "pending_review", "blocked"] {
            let resp = http
                .post(format!("{base}/spec-queue/{}/review", spec.id))
                .json(&json!({ "status": status }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "status {status}");
        }
        // The entry is untouched by the rejected attempts
        assert_eq!(
            store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::PendingReview
        );

        let resp = http
            .post(format!("{base}/spec-queue/{}/review", SpecId::new()))
            .json(&json!({"status": "approved"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn spec_queue_listing_and_reorder() {
        let (store, project) = store_with_project().await;
        let (task_a, spec_a) = insert_spec(&store, &project, 1).await;
        let (_, spec_b) = insert_spec(&store, &project, 2).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let queue: Vec<SpecQueueItem> = http
            .get(format!("{base}/spec-queue"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].entry.spec_id, spec_a.id);
        assert_eq!(queue[0].task_id, task_a.id);

        let queue: Vec<SpecQueueItem> = http
            .post(format!("{base}/spec-queue/reorder"))
            .json(&json!({"spec_ids": [spec_b.id, spec_a.id]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(queue[0].entry.spec_id, spec_b.id);
        assert_eq!(queue[0].entry.rank, Some(1));
        assert_eq!(queue[1].entry.rank, Some(2));

        let resp = http
            .post(format!("{base}/spec-queue/reorder"))
            .json(&json!({"spec_ids": [SpecId::new()]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn mode_roundtrip() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let body: Value = http
            .get(format!("{base}/mode"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["mode"], "pause");

        let body: Value = http
            .post(format!("{base}/mode"))
            .json(&json!({"mode": "play"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["mode"], "play");
        assert_eq!(store.get_mode().await.unwrap(), Mode::Play);

        let body: Value = http
            .get(format!("{base}/mode"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["mode"], "play");

        let resp = http
            .post(format!("{base}/mode"))
            .json(&json!({"mode": "fast-forward"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let events = store.all_events().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].payload,
            EventPayload::ModeChanged {
                from: Mode::Pause,
                to: Mode::Play
            }
        );
    }

    /// The `/briefings` wire shape: always all three sections, snake_case
    /// keys, RFC3339 timestamps, and — without a briefing service attached —
    /// stored copies only, never a regeneration.
    #[tokio::test]
    async fn briefings_serve_all_three_sections_from_storage() {
        use crate::models::{Briefing, BriefingSection};

        let store = Arc::new(Store::open_in_memory().await.unwrap());
        store
            .upsert_briefing(&Briefing {
                section: BriefingSection::Changes,
                content: "PR [#7](https://github.com/a/b/pull/7) is stale.".into(),
                generated_at: Utc::now(),
                event_high_water: 3,
            })
            .await
            .unwrap();
        let base = spawn(store.clone()).await;

        let body: Vec<Value> = reqwest::get(format!("{base}/briefings"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body.len(), 3);
        let sections: Vec<&str> = body
            .iter()
            .map(|b| b["section"].as_str().unwrap())
            .collect();
        assert_eq!(sections, vec!["state_of_project", "changes", "issues"]);

        let changes = &body[1];
        assert_eq!(
            changes["content"].as_str().unwrap(),
            "PR [#7](https://github.com/a/b/pull/7) is stale."
        );
        assert_eq!(changes["stale"], Value::Bool(false));
        assert_eq!(changes["regenerating"], Value::Bool(false));
        assert!(changes["generated_at"].as_str().unwrap().contains('T'));

        let never_generated = &body[0];
        assert_eq!(never_generated["content"], Value::Null);
        assert_eq!(never_generated["stale"], Value::Bool(true));

        // No service attached: nothing regenerated behind the read.
        assert_eq!(store.list_briefings().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn events_catch_up() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        for i in 0..5 {
            store
                .append_event(EventPayload::Note {
                    source: "test".into(),
                    message: format!("{i}"),
                })
                .await
                .unwrap();
        }
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let all: Vec<Event> = http
            .get(format!("{base}/events"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all.len(), 5);

        let tail: Vec<Event> = http
            .get(format!("{base}/events?since={}", all[2].seq))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].seq, all[2].seq);

        let limited: Vec<Event> = http
            .get(format!("{base}/events?since=0&limit=2"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].seq, all[0].seq);

        let resp = http
            .get(format!("{base}/events?limit=0"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn events_stream_delivers_reorder() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 1, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        // send() resolves once the SSE response head is in, which is after the
        // handler has subscribed — so the reorder below can't be missed.
        let mut stream = http
            .get(format!("{base}/events/stream"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            stream
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        http.post(format!("{base}/queue/reorder"))
            .json(&json!({"task_ids": [task.id]}))
            .send()
            .await
            .unwrap();

        let mut body = String::new();
        let event = loop {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
                .await
                .expect("sse chunk timed out")
                .unwrap()
                .expect("sse stream closed");
            body.push_str(&String::from_utf8_lossy(&chunk));
            if body.ends_with("\n\n")
                && let Some(line) = body.lines().find_map(|l| l.strip_prefix("data: "))
            {
                break serde_json::from_str::<Event>(line).unwrap();
            }
        };

        assert_eq!(
            event.payload,
            EventPayload::QueueReordered {
                task_ids: vec![task.id]
            }
        );
    }

    #[tokio::test]
    async fn missing_ids_are_404() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store).await;
        let http = reqwest::Client::new();

        for path in [
            format!("/tasks/{}", TaskId::new()),
            format!("/sessions/{}", SessionId::new()),
            format!("/specs/{}", SpecId::new()),
        ] {
            let resp = http.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(resp.status(), 404, "{path}");
            let body: Value = resp.json().await.unwrap();
            assert!(body["error"].is_string());
        }
    }

    #[tokio::test]
    async fn sessions_and_specs_listings() {
        let (store, project) = store_with_project().await;
        let (task, spec) = insert_spec(&store, &project, 1).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let sessions: Vec<Session> = http
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].task_id, task.id);

        let fetched: Session = http
            .get(format!("{base}/sessions/{}", sessions[0].id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fetched, sessions[0]);

        let specs: Vec<Spec> = http
            .get(format!("{base}/specs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(specs, vec![spec.clone()]);

        let fetched: Spec = http
            .get(format!("{base}/specs/{}", spec.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fetched, spec);
    }

    // --- transcripts (#759) ---

    /// A running session with no transcript yet.
    async fn seed_session(store: &Arc<Store>) -> SessionId {
        let (_, project) = (store, {
            let project = Project {
                id: ProjectId::new(),
                repo_owner: "o".into(),
                repo_name: "r".into(),
                added_at: Utc::now(),
            };
            store.insert_project(&project).await.unwrap();
            project
        });
        let task = insert_task(store, &project, 1, 0).await;
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: None,
            branch: String::new(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
        };
        store.insert_session(&session).await.unwrap();
        session.id
    }

    #[tokio::test]
    async fn transcript_reads_page_and_honour_inclusive_since() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let session_id = seed_session(&store).await;
        let batch: Vec<_> = (1..=5)
            .map(|i| (crate::models::TranscriptStream::Stdout, format!("line {i}")))
            .collect();
        store
            .append_transcript_lines(&session_id, &batch)
            .await
            .unwrap();

        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let all: Vec<TranscriptLine> = http
            .get(format!("{base}/sessions/{session_id}/transcript"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].line, "line 1");

        // Inclusive, like /events?since=.
        let tail: Vec<TranscriptLine> = http
            .get(format!("{base}/sessions/{session_id}/transcript?since=4"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(tail.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![4, 5]);

        // An unknown session is a 404, not an empty array — those mean
        // different things to a client.
        let resp = http
            .get(format!("{base}/sessions/sess_nope/transcript"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let resp = http
            .get(format!("{base}/sessions/{session_id}/transcript?limit=0"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// A session with no lines is "nothing recorded", not an error.
    #[tokio::test]
    async fn an_empty_transcript_is_an_empty_array() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let session_id = seed_session(&store).await;
        let base = spawn(store.clone()).await;

        let lines: Vec<TranscriptLine> = reqwest::Client::new()
            .get(format!("{base}/sessions/{session_id}/transcript"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(lines.is_empty());
    }

    /// The regression guard for the SSE replay: one limit-sized page followed
    /// by the live tail would silently skip everything in between. The count is
    /// an exact multiple of the page size, which is the boundary where a naive
    /// loop stops early.
    #[tokio::test]
    async fn transcript_stream_replays_past_one_page_without_a_hole() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let session_id = seed_session(&store).await;

        let total = (MAX_TRANSCRIPT_LIMIT * 2) as usize;
        let batch: Vec<_> = (1..=total)
            .map(|i| (crate::models::TranscriptStream::Stdout, format!("line {i}")))
            .collect();
        store
            .append_transcript_lines(&session_id, &batch)
            .await
            .unwrap();

        let base = spawn(store.clone()).await;
        let mut stream = reqwest::Client::new()
            .get(format!("{base}/sessions/{session_id}/transcript/stream"))
            .send()
            .await
            .unwrap();

        let mut body = String::new();
        let mut seqs: Vec<i64> = Vec::new();
        while seqs.len() < total {
            let chunk = tokio::time::timeout(Duration::from_secs(30), stream.chunk())
                .await
                .expect("sse chunk timed out")
                .unwrap()
                .expect("sse stream closed early");
            body.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = body.find("\n\n") {
                let frame: String = body.drain(..end + 2).collect();
                if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) {
                    seqs.push(serde_json::from_str::<TranscriptLine>(data).unwrap().seq);
                }
            }
        }

        // Dense 1..=total with nothing skipped in the middle.
        assert_eq!(seqs.len(), total);
        assert_eq!(seqs, (1..=total as i64).collect::<Vec<_>>());
    }

    /// Subscribe-then-backfill: a line appended while the handler is reading
    /// history must arrive on the live channel rather than fall down the gap.
    #[tokio::test]
    async fn transcript_stream_tails_lines_appended_after_it_attached() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let session_id = seed_session(&store).await;
        store
            .append_transcript_lines(
                &session_id,
                &[(crate::models::TranscriptStream::Stdout, "historic".into())],
            )
            .await
            .unwrap();

        let base = spawn(store.clone()).await;
        let mut stream = reqwest::Client::new()
            .get(format!("{base}/sessions/{session_id}/transcript/stream"))
            .send()
            .await
            .unwrap();

        store
            .append_transcript_lines(
                &session_id,
                &[(crate::models::TranscriptStream::Stderr, "live".into())],
            )
            .await
            .unwrap();

        let mut body = String::new();
        let mut lines: Vec<TranscriptLine> = Vec::new();
        while lines.len() < 2 {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
                .await
                .expect("sse chunk timed out")
                .unwrap()
                .expect("sse stream closed early");
            body.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = body.find("\n\n") {
                let frame: String = body.drain(..end + 2).collect();
                if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) {
                    lines.push(serde_json::from_str(data).unwrap());
                }
            }
        }
        assert_eq!(lines[0].line, "historic");
        assert_eq!(lines[1].line, "live");
        assert_eq!(
            lines[1].seq, 2,
            "no duplicate or skipped seq at the handoff"
        );
    }

    /// `GET /events` without `since` returns the newest N — a fold over that
    /// page fabricates a quiet week once real events scroll off it. This test
    /// first proves the naive read genuinely undercounts (otherwise it proves
    /// nothing), then runs the client's exact paging loop and asserts it
    /// reconstructs the log gap-free and dupe-free.
    #[tokio::test]
    async fn paging_events_reconstructs_the_log_that_newest_n_truncates() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        // 3 countable ingests, then enough noise to push them off a
        // newest-100 default page.
        for i in 0..3 {
            store
                .append_event(EventPayload::TaskIngested {
                    task_id: TaskId::from_raw(format!("task_{i}")),
                    project_id: ProjectId::from_raw("proj_1"),
                })
                .await
                .unwrap();
        }
        for i in 0..150 {
            store
                .append_event(EventPayload::Note {
                    source: "test".into(),
                    message: format!("noise {i}"),
                })
                .await
                .unwrap();
        }
        let base = spawn(store).await;
        let http = reqwest::Client::new();

        let newest_page: Vec<Value> = http
            .get(format!("{base}/events"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let naive_count = newest_page
            .iter()
            .filter(|e| e["payload"]["kind"] == "task_ingested")
            .count();
        assert_eq!(
            naive_count, 0,
            "the naive newest-N fold must genuinely undercount for this test to prove anything"
        );

        // The client's paging loop: since = high_water + 1, filter > high_water.
        let mut log: Vec<Value> = Vec::new();
        let mut high_water: i64 = 0;
        loop {
            let page: Vec<Value> = http
                .get(format!("{base}/events?since={}&limit=50", high_water + 1))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let fresh: Vec<Value> = page
                .iter()
                .filter(|e| e["seq"].as_i64().unwrap() > high_water)
                .cloned()
                .collect();
            if let Some(last) = fresh.last() {
                high_water = last["seq"].as_i64().unwrap();
            }
            let done = page.len() < 50;
            log.extend(fresh);
            if done {
                break;
            }
        }
        assert_eq!(log.len(), 153, "gap-free and dupe-free");
        let seqs: Vec<i64> = log.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
        assert_eq!(seqs, (1..=153).collect::<Vec<_>>(), "contiguous from 1");
        let paged_count = log
            .iter()
            .filter(|e| e["payload"]["kind"] == "task_ingested")
            .count();
        assert_eq!(paged_count, 3, "matches the hand count");
    }

    /// `GET /tasks` reconciles away closed+concluded work; `GET /tasks/{id}`
    /// must not — shipped work is exactly the work whose issue has closed,
    /// and dashboards still need its title.
    #[tokio::test]
    async fn retired_tasks_stay_reachable_by_id() {
        let (store, project) = store_with_project().await;
        let now = Utc::now();
        let retired = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: 900,
            title: "shipped and closed".into(),
            body: String::new(),
            labels: vec![],
            gh_state: GhState::Closed,
            state: TaskState::Done,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
        };
        store.insert_task(&retired).await.unwrap();
        let base = spawn(store).await;
        let http = reqwest::Client::new();

        let listed: Vec<Task> = http
            .get(format!("{base}/tasks"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !listed.iter().any(|t| t.id == retired.id),
            "the working set hides retired work"
        );

        let fetched: Task = http
            .get(format!("{base}/tasks/{}", retired.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fetched.title, "shipped and closed");
    }

    /// The wire timestamp form the Swift date decoder is written against:
    /// RFC3339 with a `Z` suffix (chrono's serde), not `+00:00`.
    #[tokio::test]
    async fn event_timestamps_are_rfc3339_zulu_on_the_wire() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        store
            .append_event(EventPayload::Note {
                source: "test".into(),
                message: "tick".into(),
            })
            .await
            .unwrap();
        let base = spawn(store).await;

        let events: Vec<Value> = reqwest::Client::new()
            .get(format!("{base}/events"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let ts = events[0]["timestamp"].as_str().unwrap();
        assert!(
            ts.ends_with('Z') && ts.contains('T'),
            "expected RFC3339 Zulu, got {ts}"
        );
    }
}
