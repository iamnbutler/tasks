//! Headless HTTP control API.
//!
//! This is the first (and for now only) interface onto the store: Claude Code,
//! the CLI and any later UI all drive Tasks through these routes. Reads are
//! plain JSON projections of the store; writes append events so anything
//! watching `/events/stream` sees the whole picture.
//!
//! Binds to loopback only — there is no authentication.

use std::convert::Infallible;
use std::future::IntoFuture;
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

use tasks_api::http::DecisionReconciliation;
use tasks_api::http::{
    AbandonPullRequest, AutonomyNotice, BuildDetail, BuildNowRequest, BuildRequest, CancelAck,
    CancelAllResponse, CancelRunRequest, CaptureIssue, CloseTaskRequest, CommentRequest,
    CreateProject, EditIssueRequest, ErrorResponse, GitHubHold, LabelInfo, MergePullRequest,
    ModeResponse, RejectedBundle, ReopenTaskRequest, ReorderQueue, ReorderSpecQueue,
    ReviewCommentRequest, ReviewRequest, ScoutRequest, SendMessage, ServerStatus, SetCharter,
    SetLabelsRequest, SetMode, SetProjectStatus, SettleDecisionRequest, ShadowAck, Viewer,
};

use crate::bundles::RejectedBundles;
use crate::events::{Event, EventPayload};
use crate::github::{GhIssue, GitHubClient};
use crate::github_health::GitHubHealth;
use crate::models::{
    Actor, Build, BuildId, Capability, CharterEntry, CharterLevel, ChatRole, CloseReason,
    Complexity, Decision, DecisionAction, DecisionInput, DecisionState, Directions, GhState, Mode,
    OrchestratorMessage, OrchestratorSessionInfo, Project, ProjectId, ProjectStatus, RunKind,
    ScoutNotes, Session, SessionId, SessionStatus, Spec, SpecId, SpecQueueItem, SpecQueueStatus,
    Task, TaskId, TranscriptLine, TranscriptOwner,
};
use crate::store::{
    ACTOR_HEADER, ActorClaim, MESSAGE_PAGE_DEFAULT, MESSAGE_PAGE_MAX, Store, StoreError,
    require_rationale,
};

/// How many events `/events` returns when the caller doesn't ask for a count.
const DEFAULT_EVENT_LIMIT: i64 = 100;

/// Interval between SSE keep-alive comments.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// How long [`serve_on`] waits for open connections once the drain is done,
/// before severing whatever is left and releasing the port.
///
/// A graceful shutdown alone cannot end this process, because half the API is
/// streams: `/events/stream`, `/orchestrator/stream` and the two transcript
/// tails only finish when the *client* hangs up, and the app holds all of them
/// open for as long as it is running. So "wait for connections to close" means
/// "wait for the user to quit the app" — the server sat there until `reload`
/// SIGKILLed it at 75s, on every single restart.
///
/// Two seconds, and the arithmetic is the reason: the drain is bounded at
/// 10 + 30 + 30 = 70s and [`crate::reload`] SIGKILLs at 75s, so this is what
/// fits in the remainder. It buys an ordinary request the chance to finish;
/// it deliberately does not try to outlast a stream, because no grace can.
///
/// Severing them is safe in a way that waiting is not: an SSE client is a
/// tailer, `/events/stream` resumes from `?since=`, and the successor is
/// already binding the port. A dropped stream costs a reconnect; not exiting
/// costs the pidfile cleanup, the 75s, and — when the drain needs its full
/// budget — the teardown SIGKILL lands in the middle of.
const CONNECTION_GRACE: Duration = Duration::from_secs(2);

/// Transcript lines `/sessions/{id}/transcript` returns without an explicit
/// `limit`, and the ceiling on one the caller asks for. The SSE replay pages at
/// the maximum until it catches up.
const DEFAULT_TRANSCRIPT_LIMIT: i64 = 500;

const MAX_TRANSCRIPT_LIMIT: i64 = 2000;

/// Source tag on the `Note` a `POST /mode` carrying a reason appends. One
/// tag, whoever set the mode: the message says which act it was.
const MODE_SOURCE: &str = "mode";

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    /// The caller is not allowed to do this. Today that means the charter
    /// says a capability is off, or the orchestrator reached for something
    /// only the human may set.
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Internal(String),
    /// The server is running without something this route needs — today, a
    /// GitHub token. Distinct from an internal error: nothing went wrong, the
    /// capability simply is not configured, and the caller should stop asking
    /// rather than retry.
    #[error("{0}")]
    Unavailable(String),
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
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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

/// The services a router is given besides the store: the ones that need
/// credentials, background work, or a filesystem the process owns.
///
/// A struct rather than positional arguments. Two options read fine; the
/// third is where `serve_on(listener, store, None, None, None, shutdown)`
/// stops saying anything about what is missing — and every one of them is an
/// `Option<Arc<_>>`, so transposing two is a silent behaviour change rather
/// than a type error.
///
/// Each is optional so `router(store)` (tests, embedded uses) keeps working,
/// and each absence has a defined answer rather than a pretended one: without
/// a GitHub client the endpoints that write upstream answer 503, and without
/// a bundle service `GET /bundles` answers 503 — never `[]`, because "nothing
/// was preserved" is the one wrong answer to give about a directory nobody
/// looked in.
#[derive(Clone, Default)]
pub struct Services {
    pub github: Option<Arc<GitHubClient>>,
    pub bundles: Option<Arc<RejectedBundles>>,
    /// The record the poller writes and the two dispatchers read. Absent means
    /// this router has no dispatchers behind it, which `GET /status` reports as
    /// no hold — honest, because a router with nothing to dispatch is not
    /// holding anything back.
    pub github_health: Option<Arc<GitHubHealth>>,
    /// The update watch the two dispatchers consult. Absent for the same
    /// reason as `github_health`: a router with no dispatchers holds nothing.
    pub updates: Option<Arc<crate::updates::UpdateWatch>>,
    /// The vm-pool capacity record the two dispatchers write and read. Absent
    /// for the same reason again — and, unlike the other two, it is only ever
    /// written by a gate, so a router with no dispatchers behind it would have
    /// nothing to report even if it held one.
    pub pool_health: Option<Arc<crate::pool_health::PoolHealth>>,
    /// Who the server's own GitHub credential is, remembered for a while.
    ///
    /// **The only non-`Option` field here**, and deliberately: every other
    /// service's absence has a distinct answer (a 503, no hold), and this
    /// one's does not — a router with no GitHub client answers
    /// `Unauthenticated`, which is the same thing an empty cache in front of
    /// no client would answer. There is nothing for an `Option` to say.
    pub viewer: Arc<crate::viewer::ViewerCache>,
}

/// Router state: the store plus [`Services`].
#[derive(Clone)]
pub struct AppState {
    store: Arc<Store>,
    services: Services,
}

impl FromRef<AppState> for Arc<Store> {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

impl FromRef<AppState> for Option<Arc<GitHubClient>> {
    fn from_ref(state: &AppState) -> Self {
        state.services.github.clone()
    }
}

impl FromRef<AppState> for Option<Arc<RejectedBundles>> {
    fn from_ref(state: &AppState) -> Self {
        state.services.bundles.clone()
    }
}

impl FromRef<AppState> for Option<Arc<GitHubHealth>> {
    fn from_ref(state: &AppState) -> Self {
        state.services.github_health.clone()
    }
}

impl FromRef<AppState> for Option<Arc<crate::updates::UpdateWatch>> {
    fn from_ref(state: &AppState) -> Self {
        state.services.updates.clone()
    }
}

impl FromRef<AppState> for Option<Arc<crate::pool_health::PoolHealth>> {
    fn from_ref(state: &AppState) -> Self {
        state.services.pool_health.clone()
    }
}

impl FromRef<AppState> for Arc<crate::viewer::ViewerCache> {
    fn from_ref(state: &AppState) -> Self {
        state.services.viewer.clone()
    }
}

/// Build the API router over a store alone. Exposed separately from [`serve`]
/// so tests can bind their own listener.
pub fn router(store: Arc<Store>) -> Router {
    router_with_services(store, Services::default())
}

/// Build the full API router. `serve` passes the GitHub client so issue
/// writes can go through the server rather than through an agent's own
/// credential, and the bundle service so a preserved implementation can be
/// found without an `ls` on the server host.
///
/// Every route goes through [`crate::loopback::guard`], which refuses a
/// request that names a host other than this machine's loopback or that
/// carries an `Origin` header at all. Both constructors come through here, so
/// a test that builds a router directly is guarded like the served one.
pub fn router_with_services(store: Arc<Store>, services: Services) -> Router {
    routes(store, services).layer(axum::middleware::from_fn(crate::loopback::guard))
}

/// The route list, unguarded.
///
/// Split from [`router_with_services`] deliberately: a `.layer()` chained
/// onto the end of a 120-line route list is one line a later route can be
/// appended *after*, silently unguarded. With the split, a route added here
/// is guarded by construction — including one nobody has written yet, which
/// is what `the_guard_covers_a_path_with_no_route` pins by asserting that an
/// unrouted path answers 403 rather than 404.
fn routes(store: Arc<Store>, services: Services) -> Router {
    Router::new()
        // First on purpose: no state, no store, no auth — the one route that
        // answers while everything else might still be wrong.
        .route("/version", get(get_version))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{project_id}/status", post(set_project_status))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{task_id}", get(get_task))
        .route("/tasks/{task_id}/queue", post(queue_task))
        .route("/tasks/{task_id}/dequeue", post(dequeue_task))
        .route("/tasks/{task_id}/scout", post(scout_task_now))
        .route("/tasks/{task_id}/build-now", post(build_task_now))
        .route("/tasks/{task_id}/close", post(close_task))
        .route("/tasks/{task_id}/reopen", post(reopen_task))
        .route("/issues", post(capture_issue))
        .route("/issues/{number}/comments", post(comment_on_work))
        .route("/issues/{number}/edit", post(edit_issue))
        .route("/issues/{number}/labels", post(set_issue_labels))
        .route("/labels", get(list_labels))
        .route(
            "/pull-requests/{number}/review-comments",
            post(create_review_comment),
        )
        .route("/pull-requests/{number}/merge", post(merge_pull_request))
        .route("/pull-requests/{number}/close", post(abandon_pull_request))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{session_id}", get(get_session))
        .route("/sessions/{session_id}/notes", get(get_session_notes))
        .route("/sessions/{session_id}/cancel", post(cancel_session))
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
        .route("/decisions/{seq}/reconcile", get(reconcile_decision))
        .route("/decisions/{seq}/settle", post(settle_decision))
        .route("/charter", get(get_charter))
        .route("/charter/{capability}", post(set_charter))
        .route("/autonomy-notice", get(get_autonomy_notice))
        .route("/autonomy-notice/ack", post(acknowledge_autonomy_notice))
        .route("/builds/{build_id}", get(get_build))
        .route("/builds/{build_id}/cancel", post(cancel_build))
        .route("/runs/cancel-all", post(cancel_all_runs))
        .route(
            "/builds/{build_id}/bundle",
            get(get_build_bundle).delete(delete_build_bundle),
        )
        .route("/bundles", get(list_bundles))
        .route("/builds/{build_id}/transcript", get(list_build_transcript))
        .route(
            "/builds/{build_id}/transcript/stream",
            get(stream_build_transcript),
        )
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
        .route("/viewer", get(get_viewer))
        .route("/status", get(get_status))
        .route("/mode", get(get_mode).post(set_mode))
        .route("/queue/reorder", post(reorder_queue))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .with_state(AppState { store, services })
}

/// Serve the API on loopback at `port`. Runs until the process is killed.
pub async fn serve(store: Arc<Store>, port: u16) -> std::io::Result<()> {
    serve_with_shutdown(store, Services::default(), port, std::future::pending()).await
}

/// When this process started serving, stamped once by [`serve_on`].
///
/// A static rather than router state so [`router`] keeps its shape for the
/// tests that build one directly: a router with no server around it still
/// answers `/status`, dating itself from the first call.
static SERVING_SINCE: std::sync::OnceLock<chrono::DateTime<Utc>> = std::sync::OnceLock::new();

/// When this process began serving (first call wins).
pub fn serving_since() -> chrono::DateTime<Utc> {
    *SERVING_SINCE.get_or_init(Utc::now)
}

/// Take the API's port on loopback.
///
/// Split from [`serve_on`] so a caller can fail fast on a port clash — before
/// it starts resuming work — while still holding the socket for as long as it
/// likes afterwards. `tasks serve` uses that to keep answering through its
/// whole shutdown drain, releasing the port last of all.
pub async fn bind(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    // The build is logged where the port is taken — the first line a running
    // server prints about itself, and the one an operator reads when a client
    // reports a version mismatch.
    info!(
        addr = %listener.local_addr()?,
        version = crate::version::VERSION,
        commit = crate::version::COMMIT,
        "tasks api listening"
    );
    Ok(listener)
}

/// Serve the API on an already-bound listener until `shutdown` resolves, then
/// stop accepting connections and let the in-flight ones drain — for at most
/// [`CONNECTION_GRACE`], after which whatever is still open is severed.
///
/// The port is released when this returns — so anything a caller wants to
/// finish while the API is still up belongs inside `shutdown`, not after this
/// call.
///
/// The bound is not a tuning knob, it is the exit condition. Waiting on the
/// clients alone never terminates while any of them is tailing a stream, and
/// every one of this server's SSE routes is unbounded by construction; see
/// [`CONNECTION_GRACE`]. Bounding it here rather than at the four stream
/// handlers is deliberate: a handler that learns to stop is one endpoint's
/// fix, and the next long-lived route reintroduces the hang. This is the
/// property — *a shutdown terminates* — and it holds for routes nobody has
/// written yet.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    store: Arc<Store>,
    services: Services,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Stamped here rather than in `serve_with_shutdown`, because `tasks serve`
    // now binds and serves in two steps and never goes through that helper —
    // stamping there would leave `/status` uptime dating from the first call
    // instead of from the moment this process started serving.
    serving_since();

    // The caller's `shutdown` is the drain, and it is the thing whose end
    // starts the clock — not the signal that began it. Waiting from the signal
    // would charge the connection grace against the drain's own budget.
    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel();
    let shutdown = async move {
        shutdown.await;
        let _ = drained_tx.send(());
    };

    let serve = axum::serve(listener, router_with_services(store, services))
        .with_graceful_shutdown(shutdown)
        .into_future();
    tokio::pin!(serve);

    tokio::select! {
        // Biased so that a server which closed its connections inside the
        // grace reports its own result: the timer arm returning `Ok(())` for
        // an accept loop that actually failed would swallow the error.
        biased;
        result = &mut serve => result,
        () = async {
            // A `RecvError` means the sender was dropped without sending,
            // which happens only when `serve` has already returned and taken
            // the shutdown future with it — the other arm owns that case, so
            // this one must not fire a grace period against a live server.
            if drained_rx.await.is_err() {
                std::future::pending::<()>().await
            }
            tokio::time::sleep(CONNECTION_GRACE).await;
        } => {
            warn!(
                grace_secs = CONNECTION_GRACE.as_secs(),
                "connections still open after the drain; severing them and \
                 releasing the port. A tailing client never closes on its own \
                 — an open app is the ordinary case here, not an anomaly — \
                 and waiting one out is what a shutdown cannot do"
            );
            Ok(())
        }
    }
}

/// Bind and serve in one call. [`bind`] + [`serve_on`] for callers that do not
/// need to do anything in between.
pub async fn serve_with_shutdown(
    store: Arc<Store>,
    services: Services,
    port: u16,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    serve_on(bind(port).await?, store, services, shutdown).await
}

// --- version ---

/// Which build is running, and the oldest client it expects to speak to.
///
/// No `State` and no store access, deliberately: this is the route a client
/// preflights before anything else, and the one a restart can poll to find out
/// whether the new process is up *and* is the build that was just made.
async fn get_version() -> Json<tasks_api::version::VersionInfo> {
    Json(crate::version::info())
}

// --- projects ---

/// Every project, including archived ones.
///
/// Archived projects are returned deliberately: a task whose project is
/// archived still carries that `project_id`, and a filtered list would leave
/// the row with no repo to name. Hiding them is a view concern and belongs in
/// the view — which sorts them last rather than dropping them, so a repo you
/// cannot select is not a repo you cannot un-archive.
async fn list_projects(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(store.list_projects().await?))
}

/// Trim the incidental punctuation a human pastes around a repo name, so
/// `owner/repo`, ` owner/repo ` and `owner/repo/` are one project rather than
/// three.
///
/// The server normalizes rather than the client, so there is one parser: a
/// second one in the app would be a second thing to keep in step.
fn normalize_repo(part: &str) -> String {
    part.trim().trim_matches('/').trim().to_string()
}

/// `POST /projects` — start tracking a repository.
///
/// **Human-only, and not charter-gated**, on the `build-now` precedent: adding
/// a repo commits VM hours and authorises pull requests against somebody's
/// repository. That is not a unit of work *inside* the pipeline — it decides
/// what the pipeline is pointed at — and none of the nine capabilities
/// describes it. If it is ever granted it wants its own named capability.
///
/// Nothing is dispatched by this: ingested issues land in `backlog`, and
/// backlog never dispatches.
async fn create_project(
    State(store): State<Arc<Store>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateProject>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    if actor_of(&store, &headers)? != Actor::Human {
        return Err(ApiError::Forbidden(
            "adding a repository is the human's alone: it commits VM hours and authorises \
             pull requests against somebody's repository, and no charter capability covers \
             that. Propose it to the human."
                .into(),
        ));
    }
    let repo_owner = normalize_repo(&body.repo_owner);
    let repo_name = normalize_repo(&body.repo_name);
    if repo_owner.is_empty() || repo_name.is_empty() {
        return Err(ApiError::BadRequest(
            "repo_owner and repo_name must be non-empty".into(),
        ));
    }
    // Case-insensitively, because `UNIQUE(repo_owner, repo_name)` is not:
    // `Owner/Repo` beside `owner/repo` would be two projects for one repo, and
    // then `resolve_project` can no longer answer "the only one there is".
    if let Some(existing) = store.find_project_by_repo(&repo_owner, &repo_name).await? {
        return Err(ApiError::BadRequest(format!(
            "{} is already tracked as {} ({})",
            existing.slug(),
            existing.id,
            existing.status
        )));
    }
    let project = Project {
        id: ProjectId::new(),
        repo_owner,
        repo_name,
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await?;
    store
        .append_event(EventPayload::ProjectAdded {
            project_id: project.id.clone(),
        })
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

/// `POST /projects/{project_id}/status` — pause, archive, or reactivate a repo.
///
/// **Human-only, and not charter-gated**, for the same reason as
/// `create_project`: pausing or archiving stops every scout and every build
/// for a repository.
///
/// There is no delete, and this is what stands in for one. `decisions` is
/// append-only and keyed to a project's tasks, and `tasks.project_id` is
/// `ON DELETE CASCADE`, so deleting a project would take the audit trail the
/// whole charter rests on with it.
async fn set_project_status(
    State(store): State<Arc<Store>>,
    Path(project_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetProjectStatus>,
) -> ApiResult<Json<Project>> {
    if actor_of(&store, &headers)? != Actor::Human {
        return Err(ApiError::Forbidden(
            "pausing or archiving a repository is the human's alone: it stops every scout \
             and every build for it, and no charter capability covers that. Propose it to \
             the human."
                .into(),
        ));
    }
    let status = ProjectStatus::from_str(&body.status).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "unknown status: {} — expected active, paused or archived",
            body.status
        ))
    })?;
    let id = ProjectId::from_raw(project_id);
    Ok(Json(store.set_project_status(&id, status).await?))
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

/// Largest set of directions any endpoint accepts.
///
/// Generous — this is a paragraph or two of instruction, not a document — and
/// it exists to bound what lands in an agent's context window rather than to
/// bound a column.
const MAX_DIRECTIONS_BYTES: usize = 16 * 1024;

/// Read a `directions` field off a request body.
///
/// The doubled `Option` is the whole point, and the three cases are three
/// different intentions:
///
/// - `None` — the field was absent. **Leave whatever is staged alone.** A
///   second `POST /scout` with no body must not silently unaim the run, which
///   is why "absent" cannot be spelled the same way as "clear".
/// - `Some(None)` — present but empty or whitespace. Clear it.
/// - `Some(Some(d))` — these directions, attributed to `actor`.
///
/// Over [`MAX_DIRECTIONS_BYTES`] is a **400, not a truncation**: an
/// instruction cut off halfway is a different instruction, and one the caller
/// would have no way to know they gave.
fn parse_directions(raw: Option<&str>, actor: Actor) -> ApiResult<Option<Option<Directions>>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Some(None));
    }
    if trimmed.len() > MAX_DIRECTIONS_BYTES {
        return Err(ApiError::BadRequest(format!(
            "directions are {} bytes, over the {MAX_DIRECTIONS_BYTES}-byte limit. They are \
             refused rather than shortened: an instruction cut off halfway is a different \
             instruction. If this much needs saying, it belongs in the issue or the spec, \
             which are reviewed",
            trimmed.len()
        )));
    }
    Ok(Some(Some(Directions::new(trimmed, actor))))
}

/// Pick a backlog task up into the scout queue (appended at the end).
async fn queue_task(
    State(store): State<Arc<Store>>,
    Path(task_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<ScoutRequest>>,
) -> ApiResult<Response> {
    let id = TaskId::from_raw(task_id);
    queue_under_charter(&store, &headers, &id, false, body).await
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
    headers: axum::http::HeaderMap,
    body: Option<Json<ScoutRequest>>,
) -> ApiResult<Response> {
    let id = TaskId::from_raw(task_id);
    queue_under_charter(&store, &headers, &id, true, body).await
}

/// Queue a task, or record that the orchestrator would have.
///
/// Queueing is where spend begins — a queued task becomes a Scout run —
/// which is why it is a charter capability at all. The ledger row is written
/// after the queueing rather than inside it: queueing is trivially reversible,
/// so the row is there for the budget and the audit trail, not to authorize
/// anything.
async fn queue_under_charter(
    store: &Store,
    headers: &axum::http::HeaderMap,
    id: &TaskId,
    front: bool,
    body: Option<Json<ScoutRequest>>,
) -> ApiResult<Response> {
    // Both routes took no body at all before `directions` existed, and every
    // caller that still sends none has to keep working — `Option<Json<T>>` is
    // the extractor for that, already proven here by `build_task_now`.
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let actor = actor_of(store, headers)?;
    // Parsed before anything is written, so an oversized set 400s having
    // staged nothing *and* queued nothing.
    let directions = parse_directions(body.directions.as_deref(), actor)?;
    // The one gated action with a *default* rationale, so it has to be
    // resolved before `authorize` — a check upstream of both branches cannot
    // see a rationale that does not exist yet. There is now one default rather
    // than the two there were, because the value validated has to be the value
    // recorded.
    let decision = DecisionInput {
        actor,
        rationale: body.rationale.or_else(|| {
            Some(if front {
                "scout now".into()
            } else {
                "queued".into()
            })
        }),
        evidence: body.evidence,
    };
    let authority = authorize(
        store,
        &decision,
        Capability::QueueTasks,
        DecisionAction::QueueTask,
    )
    .await?;
    if authority == Authority::Shadow {
        // Deliberately no staging on this path. Nothing was queued, so
        // directions written here would sit waiting to steer whoever queues
        // the task next — which is not what the caller asked for and not what
        // a shadow row means.
        let seq = store
            .record_decision(
                "task",
                id.as_str(),
                DecisionAction::QueueTask,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the task was not queued"));
    }
    // Staged *before* the queueing: in the other order the dispatch loop can
    // claim the task in between and start a run the directions never reached.
    if let Some(directions) = &directions {
        store.set_scout_directions(id, directions.as_ref()).await?;
    }
    let task = if front {
        store.push_task_to_front(id).await?
    } else {
        store.queue_task(id).await?
    };
    if actor == Actor::Orchestrator {
        store
            .record_decision(
                "task",
                id.as_str(),
                DecisionAction::QueueTask,
                // The caller's own words when it gave any, and the default
                // above when it gave none. `directions` is never copied in
                // here and `rationale` never reaches the Scout: one is read by
                // humans afterwards, the other is read by the agent.
                decision,
                true,
            )
            .await?;
    }
    Ok(Json(task).into_response())
}

// --- custodial writes: filing and retiring work ---

/// File an issue upstream and start tracking it.
///
/// This exists so the capability stops living outside the system. The
/// orchestrator can already file issues with its own `gh` credential — that is
/// the same side channel the `Closes #N` incident went through, and it leaves
/// no decision row, no event, and nothing to cap. Routing the write here
/// restores "GitHub writes go through the server, never through agents".
///
/// The task lands in `backlog`: capturing work and choosing to work on it are
/// separate capabilities, deliberately.
async fn capture_issue(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CaptureIssue>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot file issues".into())
    })?;
    if body.title.trim().is_empty() {
        return Err(ApiError::BadRequest("title must be non-empty".into()));
    }
    let actor = actor_of(&store, &headers)?;
    // Provenance is what makes a captured issue auditable from GitHub alone,
    // so an autonomous capture without it is refused rather than filed
    // anonymously.
    if actor == Actor::Orchestrator
        && body
            .provenance
            .as_deref()
            .is_none_or(|p| p.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "an orchestrator capture must say where the work was discovered".into(),
        ));
    }
    let project = resolve_project(&store, body.project_id).await?;
    // Built here rather than in each branch, so `authorize` can refuse a
    // rationale-less capture before the issue is filed. It has to come after
    // the `title` and `provenance` validation above, which reads `&body`.
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };

    if authorize(
        &store,
        &decision,
        Capability::CaptureWork,
        DecisionAction::CaptureWork,
    )
    .await?
        == Authority::Shadow
    {
        // Nothing exists to point at — the issue was never filed — so the
        // subject is the title. A shadow row is a record of judgment, not a
        // foreign key.
        let seq = store
            .record_decision(
                "capture",
                &body.title,
                DecisionAction::CaptureWork,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "no issue was filed"));
    }

    let rendered = issue_body(&body.body, actor, body.provenance.as_deref());
    // The subject is the title, not a task: no issue number and no task exists
    // yet, and the intent has to be on record *before* the call that would
    // create one. That is what the shadow branch above already does.
    let (seq, number) = ledgered(
        &store,
        "capture",
        &body.title,
        DecisionAction::CaptureWork,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "title": body.title,
            "labels": body.labels,
        }),
        "filing the issue failed",
        || {
            github.create_issue(
                &project.repo_owner,
                &project.repo_name,
                &body.title,
                &rendered,
                &body.labels,
            )
        },
    )
    .await?;

    // The issue exists upstream now and the ledger says so, so a failure past
    // this point loses tracking, not attribution — and the poller picks the
    // issue up on its next pass either way.
    let task = store
        .capture_issue(
            &project.id,
            GhIssue {
                number,
                title: body.title,
                body: body.body,
                labels: body.labels,
                state: GhState::Open,
                updated_at: Utc::now(),
            },
            actor,
            seq,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(task)).into_response())
}

/// Close the GitHub issue behind a task.
///
/// Returns 202, not the updated task: the task is *not* retired here. Closure
/// is GitHub's fact, and the poller observes it on its next pass through the
/// path that already handles an issue closed in a browser. Marking it locally
/// would persist a GitHub-owned fact and make a failed close look successful.
async fn close_task(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(task_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CloseTaskRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot close issues".into())
    })?;
    let reason = CloseReason::from_str(&body.reason)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown reason: {}", body.reason)))?;
    let id = TaskId::from_raw(task_id);
    let actor = actor_of(&store, &headers)?;
    // Built here rather than in each branch, so `authorize` can refuse a
    // rationale-less close before the issue is closed upstream — the same bug
    // as the capture path, on a route whose only recourse is a reopen.
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::RetireWork,
        DecisionAction::RetireWork,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "task",
                id.as_str(),
                DecisionAction::RetireWork,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the issue is still open"));
    }
    let task = store
        .get_task(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("task {id}")))?;
    let project = store
        .get_project(&task.project_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("project {}", task.project_id)))?;

    let (seq, ()) = ledgered(
        &store,
        "task",
        id.as_str(),
        DecisionAction::RetireWork,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "issue": task.gh_issue_number,
            "reason": reason.as_str(),
        }),
        "closing the issue failed",
        || {
            github.close_issue(
                &project.repo_owner,
                &project.repo_name,
                task.gh_issue_number,
                reason,
            )
        },
    )
    .await?;

    store.record_issue_closed(&id, reason, actor, seq).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Reopen the GitHub issue behind a retired task.
///
/// Symmetric with [`close_task`], and governed by the same capability: the
/// power to retire work and the power to take that back are the same power,
/// and splitting them would mean a charter could switch off the recourse while
/// leaving the mistake-making half `live`.
///
/// Returns 202 for the same reason `close_task` does — open-or-closed is
/// GitHub's fact, and the poller reads it back on its next pass.
async fn reopen_task(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(task_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ReopenTaskRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot reopen issues".into())
    })?;
    let id = TaskId::from_raw(task_id);
    let actor = actor_of(&store, &headers)?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::RetireWork,
        DecisionAction::ReopenWork,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "task",
                id.as_str(),
                DecisionAction::ReopenWork,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the issue is still closed"));
    }
    let task = store
        .get_task(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("task {id}")))?;
    let project = store
        .get_project(&task.project_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("project {}", task.project_id)))?;

    ledgered(
        &store,
        "task",
        id.as_str(),
        DecisionAction::ReopenWork,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "issue": task.gh_issue_number,
        }),
        "reopening the issue failed",
        || {
            github.reopen_issue(
                &project.repo_owner,
                &project.repo_name,
                task.gh_issue_number,
            )
        },
    )
    .await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Comment on an issue or a pull request.
///
/// The lightest write in the system and the one whose absence hurt most: an
/// agent that can form a review verdict but has nowhere to put it hands the
/// verdict back as prose, and a human re-reads and re-types work that was
/// already done. That is the shadow-mode failure arriving by a different road.
///
/// `number` is a GitHub issue-or-PR number, not a task id, because that is
/// what the thing being commented on actually is — and a PR opened by a
/// Builder has no task of its own to address.
async fn comment_on_work(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CommentRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot comment".into())
    })?;
    if body.body.trim().is_empty() {
        return Err(ApiError::BadRequest("a comment must say something".into()));
    }
    let actor = actor_of(&store, &headers)?;
    let project = resolve_project(&store, body.project_id).await?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::CommentOnWork,
        DecisionAction::CommentOnWork,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "gh",
                &number.to_string(),
                DecisionAction::CommentOnWork,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "nothing was posted"));
    }

    let text = attributed(&body.body, actor);
    let (seq, comment_id) = ledgered(
        &store,
        "gh",
        &number.to_string(),
        DecisionAction::CommentOnWork,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "number": number,
            "body": text,
        }),
        "commenting failed",
        || github.create_issue_comment(&project.repo_owner, &project.repo_name, number, &text),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "comment_id": comment_id, "decision_seq": seq })),
    )
        .into_response())
}

/// Merge a pull request.
///
/// The one write here whose recourse is a revert rather than an edit, so it
/// asks for more: the orchestrator must state a rationale, and mergeability is
/// never read from anything we stored — GitHub refuses the call itself when a
/// required check is failing or the branch conflicts, which is the check we
/// want and the only one that cannot go stale.
async fn merge_pull_request(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<MergePullRequest>,
) -> ApiResult<Response> {
    let github = github
        .ok_or_else(|| ApiError::Unavailable("no GITHUB_TOKEN: the server cannot merge".into()))?;
    let method = body.method.as_deref().unwrap_or("squash");
    if !matches!(method, "merge" | "squash" | "rebase") {
        return Err(ApiError::BadRequest(format!(
            "unknown merge method: {method} (merge, squash, or rebase)"
        )));
    }
    let actor = actor_of(&store, &headers)?;
    if actor == Actor::Orchestrator
        && body
            .rationale
            .as_deref()
            .is_none_or(|r| r.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "an autonomous merge must say why it is safe to land".into(),
        ));
    }
    let project = resolve_project(&store, body.project_id).await?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::LandBuilds,
        DecisionAction::MergeBuild,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "gh",
                &number.to_string(),
                DecisionAction::MergeBuild,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the pull request is still open"));
    }

    let (seq, sha) = ledgered(
        &store,
        "gh",
        &number.to_string(),
        DecisionAction::MergeBuild,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "number": number,
            "method": method,
        }),
        "merging failed",
        || {
            github.merge_pull_request(
                &project.repo_owner,
                &project.repo_name,
                number,
                method,
                body.commit_title.as_deref(),
            )
        },
    )
    .await?;
    Ok(Json(serde_json::json!({ "merged_sha": sha, "decision_seq": seq })).into_response())
}

/// Close a pull request without merging it.
///
/// Abandoning a Builder run throws away work that cost a VM hour, so the
/// orchestrator has to say why. No `state_reason` exists on the PR resource —
/// if the reason should be visible on GitHub, post it with
/// [`comment_on_work`] first.
async fn abandon_pull_request(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AbandonPullRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot close pull requests".into())
    })?;
    let actor = actor_of(&store, &headers)?;
    if actor == Actor::Orchestrator
        && body
            .rationale
            .as_deref()
            .is_none_or(|r| r.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "abandoning a build must say why the branch will not land".into(),
        ));
    }
    let project = resolve_project(&store, body.project_id).await?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::LandBuilds,
        DecisionAction::AbandonBuild,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "gh",
                &number.to_string(),
                DecisionAction::AbandonBuild,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the pull request is still open"));
    }

    ledgered(
        &store,
        "gh",
        &number.to_string(),
        DecisionAction::AbandonBuild,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "number": number,
        }),
        "closing the pull request failed",
        || github.close_pull_request(&project.repo_owner, &project.repo_name, number),
    )
    .await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Comment on one line of a pull request's diff.
///
/// Separate from [`comment_on_work`] because it is a different resource and a
/// different kind of statement: a thread comment is about the PR, this points
/// at code. "The `CARGO=/nonexistent-cargo` test was dropped" is worth far
/// more anchored to the line that dropped it than said in a chat log the
/// reviewer has to hold in their head until they next open the PR.
async fn create_review_comment(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ReviewCommentRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot comment".into())
    })?;
    if body.body.trim().is_empty() {
        return Err(ApiError::BadRequest("a comment must say something".into()));
    }
    let actor = actor_of(&store, &headers)?;
    let project = resolve_project(&store, body.project_id).await?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    let subject = format!("{number}#{}:{}", body.path, body.line);
    if authorize(
        &store,
        &decision,
        Capability::CommentOnWork,
        DecisionAction::ReviewComment,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "gh",
                &subject,
                DecisionAction::ReviewComment,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "nothing was posted"));
    }

    let text = attributed(&body.body, actor);
    let (seq, comment_id) = ledgered(
        &store,
        "gh",
        &subject,
        DecisionAction::ReviewComment,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "number": number,
            "path": body.path,
            "line": body.line,
            "body": text,
        }),
        "review comment failed",
        || {
            github.create_review_comment(
                &project.repo_owner,
                &project.repo_name,
                number,
                &body.path,
                body.line,
                &text,
            )
        },
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "comment_id": comment_id, "decision_seq": seq })),
    )
        .into_response())
}

/// Rewrite an issue's title or body.
///
/// The only write here that destroys rather than appends, and the reason it
/// exists anyway: an issue filed on a theory that later collapses is worse
/// than no issue, because the next reader inherits the superseded reasoning as
/// though it still held.
///
/// What keeps that safe is not a permission check. The server reads the
/// current text first and stores it on the decision, unasked — "the
/// orchestrator edited #835" is not an auditable record, the diff is, and a
/// ledger that only names the event leaves nothing to recover from.
async fn edit_issue(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<EditIssueRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot edit issues".into())
    })?;
    if body.title.is_none() && body.body.is_none() {
        return Err(ApiError::BadRequest(
            "an edit must change the title, the body, or both".into(),
        ));
    }
    let actor = actor_of(&store, &headers)?;
    if actor == Actor::Orchestrator
        && body
            .rationale
            .as_deref()
            .is_none_or(|r| r.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "an edit must say what changed and why the earlier text no longer holds".into(),
        ));
    }
    let project = resolve_project(&store, body.project_id).await?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };

    if authorize(
        &store,
        &decision,
        Capability::CurateWork,
        DecisionAction::EditIssue,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "gh",
                &number.to_string(),
                DecisionAction::EditIssue,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the issue is unchanged"));
    }

    // Read before the **intent**, not merely before the write: `evidence` is
    // the immutable half of a ledger row, so anything belonging there has to
    // be known when the row is written. A read is not an effect, so running it
    // first costs nothing and there is nothing to attribute if it fails.
    let (old_title, old_body) = github
        .issue_body(&project.repo_owner, &project.repo_name, number)
        .await
        .map_err(|e| ApiError::Internal(format!("reading the issue failed: {e}")))?;

    let evidence = serde_json::json!({
        "replaced": { "title": old_title, "body": old_body },
        "caller_evidence": decision.evidence,
    });
    let decision = DecisionInput {
        evidence: Some(evidence),
        ..decision
    };
    let (seq, ()) = ledgered(
        &store,
        "gh",
        &number.to_string(),
        DecisionAction::EditIssue,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "number": number,
            "title": body.title,
            "body": body.body,
        }),
        "editing the issue failed",
        || {
            github.update_issue(
                &project.repo_owner,
                &project.repo_name,
                number,
                body.title.as_deref(),
                body.body.as_deref(),
            )
        },
    )
    .await?;
    Ok(Json(serde_json::json!({ "decision_seq": seq })).into_response())
}

/// Replace an issue's labels.
///
/// The complete set rather than an addition, so removing one is expressible.
async fn set_issue_labels(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetLabelsRequest>,
) -> ApiResult<Response> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot set labels".into())
    })?;
    let actor = actor_of(&store, &headers)?;
    let project = resolve_project(&store, body.project_id).await?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::CurateWork,
        DecisionAction::LabelIssue,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(
                "gh",
                &number.to_string(),
                DecisionAction::LabelIssue,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "the labels are unchanged"));
    }

    ledgered(
        &store,
        "gh",
        &number.to_string(),
        DecisionAction::LabelIssue,
        &decision,
        serde_json::json!({
            "repo": format!("{}/{}", project.repo_owner, project.repo_name),
            "number": number,
            "labels": body.labels,
        }),
        "setting labels failed",
        || {
            github.set_issue_labels(
                &project.repo_owner,
                &project.repo_name,
                number,
                &body.labels,
            )
        },
    )
    .await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// The repository's label vocabulary.
///
/// A read, but it is the one that makes labelling possible at all: with no way
/// to ask what labels exist, the only honest thing a caller can do is file
/// with none, which is exactly what has been happening.
async fn list_labels(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Query(q): Query<ProjectQuery>,
) -> ApiResult<Json<Vec<LabelInfo>>> {
    let github = github.ok_or_else(|| {
        ApiError::Unavailable("no GITHUB_TOKEN: the server cannot read labels".into())
    })?;
    let project = resolve_project(&store, q.project_id).await?;
    let labels = github
        .list_labels(&project.repo_owner, &project.repo_name)
        .await
        .map_err(|e| ApiError::Internal(format!("reading labels failed: {e}")))?;
    Ok(Json(
        labels
            .into_iter()
            .map(|(name, description)| LabelInfo { name, description })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
struct ProjectQuery {
    #[serde(default)]
    project_id: Option<ProjectId>,
}

/// Sign a comment with who wrote it.
///
/// A human's comment goes up verbatim — it is theirs, under their account,
/// and a footer would be noise. An orchestrator comment is the system talking
/// in public under the owner's name, and a reader on GitHub with no access to
/// the ledger should not have to guess which it was.
fn attributed(body: &str, actor: Actor) -> String {
    // `System` is unreachable here — the poller writes no comments — and it is
    // grouped with the orchestrator because "not the human" is the property
    // this cares about.
    match actor {
        Actor::Human => body.to_string(),
        Actor::Orchestrator | Actor::System => {
            format!("{body}\n\n---\nPosted by the Tasks orchestrator.")
        }
    }
}

/// The answer to a shadowed write: nothing changed, and here is the ledger
/// row saying what would have.
///
/// A distinct shape rather than the usual body, because returning the normal
/// success response for a call that did nothing is how a shadow evaluation
/// quietly becomes a lie. Only the orchestrator can ever see this — a human
/// is never shadowed — so no typed client has to handle it.
/// Run an effect that lands in somebody else's system, with its intent
/// already on record — and settle the record against what actually happened.
///
/// #957 closed the half of the attribution gap that can be *refused*: every
/// gated handler applies `require_rationale` inside [`authorize`], before it
/// touches GitHub, so a 4xx is a genuine no-op. This closes the half that
/// cannot be. Ten sites ran the effect and *then* the `record_decision`
/// explaining it, so a SQLite error, a panic or a SIGKILL in between left a
/// real artifact upstream that nothing in the ledger accounts for (#964).
///
/// Recording first stays refused: a row claiming an effect a failed call never
/// had makes every row suspect, where a missing row leaves one artifact
/// unexplained. The window is *represented* instead — `pending` before,
/// `applied` or `annulled` after.
///
/// **Taking the effect as a closure is the point.** A handler nobody has
/// written yet cannot reach GitHub without its intent already being on record,
/// which is the same property that made `authorize` the right home for the
/// rationale check. It runs *after* `authorize`, never instead of it: a
/// refusal must still cost nothing, including a ledger row.
///
/// Three outcomes, decided **structurally off [`GhError::is_unavailable`] and
/// never off message text** — the same predicate `github_health` turns on:
///
/// - returned → `applied`, with what it produced in `outcome`;
/// - refused with an *answer* (4xx, 429, a shape we could not read) →
///   `annulled`, because GitHub said no and nothing reached the world;
/// - **never answered** (5xx, or no response at all) → **stays pending**,
///   because we do not know, and saying so is the whole point.
///
/// A settle that itself fails is logged and **not** propagated. The effect
/// happened; a 500 here sends a well-behaved caller into the retry that files
/// a second issue, which is the #957 failure. What it leaves behind is a
/// durable, attributed, unconfirmed row — the outcome this function exists to
/// guarantee — and `ObligationKind::ReconcileDecision` chases it.
#[allow(clippy::too_many_arguments)]
async fn ledgered<T, F, Fut>(
    store: &Store,
    subject_kind: &str,
    subject_id: &str,
    action: DecisionAction,
    decision: &DecisionInput,
    intent: serde_json::Value,
    failed: &str,
    effect: F,
) -> ApiResult<(i64, T)>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, crate::github::GhError>>,
    T: serde::Serialize,
{
    let seq = store
        .record_intent(subject_kind, subject_id, action, decision, Some(&intent))
        .await?;
    match effect().await {
        Ok(value) => {
            // Not `?`: a hidden serialization error here would leave a
            // settled effect unsettled, for a case that cannot occur — every
            // `T` is a number, a string or `()`.
            let produced = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
            settled(
                store,
                seq,
                crate::models::DecisionState::Applied,
                serde_json::json!({ "result": produced }),
            )
            .await;
            Ok((seq, value))
        }
        Err(e) if e.is_unavailable() => {
            // GitHub never answered. The write may have landed. Leaving the
            // row pending is the only honest description, and the note is
            // what the reconciliation reads.
            if let Err(store_err) = store
                .note_decision_outcome(seq, &serde_json::json!({ "unanswered": e.to_string() }))
                .await
            {
                error!(seq, error = %store_err, "recording an unanswered effect failed");
            }
            warn!(
                seq,
                error = %e,
                "GitHub never answered; decision {seq} stays pending until it is reconciled"
            );
            Err(ApiError::Unavailable(format!(
                "{failed}: {e} — decision {seq} is recorded as pending; \
                 GET /decisions/{seq}/reconcile once GitHub is answering again"
            )))
        }
        Err(e) => {
            settled(
                store,
                seq,
                crate::models::DecisionState::Annulled,
                serde_json::json!({ "refused": e.to_string() }),
            )
            .await;
            Err(ApiError::Internal(format!("{failed}: {e}")))
        }
    }
}

/// Settle a row, loudly on failure and never fatally. See [`ledgered`].
async fn settled(
    store: &Store,
    seq: i64,
    state: crate::models::DecisionState,
    outcome: serde_json::Value,
) {
    if let Err(e) = store.settle_decision(seq, state, Some(&outcome)).await {
        error!(
            seq,
            state = state.as_str(),
            error = %e,
            "the effect happened and settling its ledger row did not — the row stays \
             pending and ReconcileDecision will chase it"
        );
    }
}

fn shadowed(decision_seq: i64, effect: &str) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(ShadowAck {
            shadowed: true,
            decision_seq,
            note: format!(
                "recorded, not applied: {effect}. This capability is in shadow —                  say what you decided and why in the conversation; the human acts on it."
            ),
        }),
    )
        .into_response()
}

/// The project a write targets: the one named, or the only one still live.
/// Guessing between several would be a coin flip with a GitHub write attached.
///
/// Archived projects do not count towards "the only one there is", or
/// archiving a repo would silently break `POST /issues` for the one that is
/// left — a 400 saying "2 projects configured" about a server with one live
/// repo reads as a bug. Naming an archived project explicitly still resolves
/// it: commenting on its open PR and closing its issue are exactly the work
/// archiving does not abandon.
async fn resolve_project(store: &Store, id: Option<ProjectId>) -> ApiResult<Project> {
    match id {
        Some(id) => store
            .get_project(&id)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("project {id}"))),
        None => {
            let mut projects = store.list_projects().await?;
            projects.retain(|p| p.status != ProjectStatus::Archived);
            match projects.len() {
                1 => Ok(projects.remove(0)),
                0 => Err(ApiError::BadRequest(
                    "no projects configured (archived ones do not count — name one with \
                     project_id)"
                        .into(),
                )),
                n => Err(ApiError::BadRequest(format!(
                    "{n} projects configured — name one with project_id"
                ))),
            }
        }
    }
}

/// The issue body as filed: what the caller wrote, plus a footer saying who
/// filed it and why they were looking. Appended server-side so it cannot be
/// left off, and kept out of the stored `body` so the poller's refresh from
/// GitHub does not fight with it.
fn issue_body(body: &str, actor: Actor, provenance: Option<&str>) -> String {
    // As in `attributed`: `System` cannot reach this — the poller files no
    // issues — and shares the orchestrator's line because the distinction that
    // matters to a reader of the issue is "not the human".
    let who = match actor {
        Actor::Orchestrator | Actor::System => "Filed by the Tasks orchestrator",
        Actor::Human => "Filed via Tasks",
    };
    match provenance.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => format!("{body}\n\n---\n{who} — {p}."),
        None => format!("{body}\n\n---\n{who}."),
    }
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

/// Salvage from a scout run that stopped early. 404 when there is none —
/// which is the normal case, since most sessions either conclude or leave
/// nothing behind.
///
/// Deliberately its own endpoint rather than a field on [`Session`]: notes run
/// to a quarter-megabyte, and `GET /sessions` is refetched on every event.
/// Just as deliberately not reachable from `/specs` or `/spec-queue` — these
/// notes are unverified exploration and there is no shape in which they should
/// reach a reviewer.
async fn get_session_notes(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<ScoutNotes>> {
    let id = SessionId::from_raw(session_id);
    store
        .get_scout_notes(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("notes for session {id}")))
}

// --- cancelling work in flight ---

/// How long `cancel_run` waits to see the run actually conclude before
/// answering.
///
/// Not a guarantee, and deliberately not one: the dispatcher's teardown is
/// bounded at 120s ([`crate::teardown::DEALLOCATE_TIMEOUT`]), so a wedged pool
/// can outlast this, and a run that got past its drain and is pushing a branch
/// is not interruptible at all. Long enough that the ordinary case reads as
/// done, short enough that nobody's HTTP client is left hanging on the unusual
/// one.
const CANCEL_SETTLE: Duration = Duration::from_secs(3);
const CANCEL_SETTLE_POLL: Duration = Duration::from_millis(100);

/// `POST /sessions/{session_id}/cancel` — stop a scout that is running.
async fn cancel_session(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Response> {
    let id = SessionId::from_raw(session_id);
    let session = store
        .get_session(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("session {id}")))?;
    if session.status != SessionStatus::Running {
        return Err(ApiError::Conflict(format!(
            "session {id} is {} — it has already concluded",
            session.status.as_str()
        )));
    }
    cancel_run(&store, &headers, RunKind::Session, id.as_str(), body).await
}

/// `POST /builds/{build_id}/cancel` — stop a build that is queued or running.
///
/// A `queued` build is cancellable too, and is the one case the handler
/// applies itself: nothing is following it, so nobody would ever read the
/// request.
async fn cancel_build(
    State(store): State<Arc<Store>>,
    Path(build_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Response> {
    let id = BuildId::from_raw(build_id);
    let build = store
        .get_build(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("build {id}")))?;
    if build.status.is_terminal() {
        return Err(ApiError::Conflict(format!(
            "build {id} is {} — it has already concluded",
            build.status.as_str()
        )));
    }
    cancel_run(&store, &headers, RunKind::Build, id.as_str(), body).await
}

/// One cancel, behind both routes.
///
/// The order of the four writes is the whole of the care here:
///
/// 1. **The run is checked first** (by the callers above): 404 for one that
///    does not exist, 409 for one that has already concluded. Nothing is
///    recorded about a run nobody can stop.
/// 2. **The ledger row before the cancel.** `record_decision` is what refuses
///    an orchestrator cancel with no rationale, and that refusal has to land
///    before any work is destroyed.
/// 3. **The durable request before the announcing event.** A crash between the
///    two costs the wake-up — which the observer's poll covers — rather than
///    the cancel.
/// 4. **Then settle**, so `concluded` is something the server observed rather
///    than something it hopes.
async fn cancel_run(
    store: &Arc<Store>,
    headers: &axum::http::HeaderMap,
    kind: RunKind,
    id: &str,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Response> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let actor = actor_of(store, headers)?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale.clone(),
        evidence: body.evidence,
    };
    let subject = kind.as_str();

    if authorize(
        store,
        &decision,
        Capability::CancelRuns,
        DecisionAction::CancelRun,
    )
    .await?
        == Authority::Shadow
    {
        let seq = store
            .record_decision(subject, id, DecisionAction::CancelRun, decision, false)
            .await?;
        return Ok(shadowed(seq, "the run is still going"));
    }

    let decision_seq = store
        .record_decision(subject, id, DecisionAction::CancelRun, decision, true)
        .await?;
    let request = store
        .request_cancel(
            kind,
            id,
            actor,
            body.rationale.as_deref(),
            Some(decision_seq),
        )
        .await?;
    store
        .append_event(EventPayload::RunCancelRequested {
            run_kind: kind,
            run_id: id.to_string(),
            actor,
            decision_seq: Some(decision_seq),
        })
        .await?;
    info!(%kind, run_id = id, actor = actor.as_str(), "cancel requested");

    // A queued build has no dispatcher parked on it, so the request would sit
    // unread forever. Conditional on the status inside the store, so losing the
    // race against the serial build loop leaves the running build's own cancel
    // path to conclude it.
    if kind == RunKind::Build {
        store
            .cancel_queued_build(&BuildId::from_raw(id.to_string()), &request.exit_reason())
            .await?;
    }

    let deadline = tokio::time::Instant::now() + CANCEL_SETTLE;
    let (concluded, status) = settle(store, kind, id, deadline).await?;
    let note = match concluded {
        true => format!("the {} stopped: it is now {status}", kind.noun()),
        // Deliberately not "teardown is underway": the other way to be here is
        // a run that got past its drain and is landing a branch, which no
        // cancel reaches.
        false => format!(
            "the cancel is recorded and the {} is still {status}; whoever is following \
             the run concludes it, and its completion event says so",
            kind.noun()
        ),
    };
    Ok(Json(CancelAck {
        run_kind: kind,
        run_id: id.to_string(),
        concluded,
        status,
        decision_seq,
        note,
    })
    .into_response())
}

/// `POST /runs/cancel-all` — stop everything that currently holds a VM.
///
/// "All" is precisely the set with a container: `running` sessions and
/// `running` builds, read from the same query `/status` reports in-flight
/// work from. A `queued` build deliberately survives — it holds no VM, it is
/// durable intent, and killing containers must not quietly rewrite the queue.
/// (Pause the mode first if the point is that nothing further starts.)
///
/// Semantically this is N single cancels and it is authorized and recorded as
/// exactly that: one capability check, then one ledger row and one durable
/// cancellation request per run, through the same writes `cancel_run` makes —
/// so each run's `exit_reason` names the actor and rationale individually,
/// and the audit trail does not have a special bulk shape to learn.
async fn cancel_all_runs(
    State(store): State<Arc<Store>>,
    headers: axum::http::HeaderMap,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Response> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let actor = actor_of(&store, &headers)?;

    let in_flight = store.in_flight().await?;
    let targets: Vec<(RunKind, String)> = in_flight
        .scouts
        .iter()
        .map(|item| (RunKind::Session, item.id.clone()))
        .chain(
            in_flight
                .builds
                .iter()
                .map(|item| (RunKind::Build, item.id.clone())),
        )
        .collect();
    if targets.is_empty() {
        return Ok(Json(CancelAllResponse {
            runs: Vec::new(),
            note: "nothing is running — no containers to kill".to_string(),
        })
        .into_response());
    }

    // One decision above the gate, cloned per target below: `authorize` has to
    // see the rationale before any run is torn down, and `record_decision`
    // takes it by value with one row per run.
    let decision = DecisionInput {
        actor,
        rationale: body.rationale.clone(),
        evidence: body.evidence.clone(),
    };
    let shadowed = authorize(
        &store,
        &decision,
        Capability::CancelRuns,
        DecisionAction::CancelRun,
    )
    .await?
        == Authority::Shadow;

    let mut issued: Vec<(RunKind, String, i64)> = Vec::with_capacity(targets.len());
    for (kind, id) in &targets {
        let decision = decision.clone();
        let decision_seq = store
            .record_decision(
                kind.as_str(),
                id,
                DecisionAction::CancelRun,
                decision,
                !shadowed,
            )
            .await?;
        if shadowed {
            continue;
        }
        store
            .request_cancel(
                *kind,
                id,
                actor,
                body.rationale.as_deref(),
                Some(decision_seq),
            )
            .await?;
        store
            .append_event(EventPayload::RunCancelRequested {
                run_kind: *kind,
                run_id: id.clone(),
                actor,
                decision_seq: Some(decision_seq),
            })
            .await?;
        info!(%kind, run_id = id.as_str(), actor = actor.as_str(), "cancel requested (cancel-all)");
        issued.push((*kind, id.clone(), decision_seq));
    }

    if shadowed {
        return Ok(Json(CancelAllResponse {
            runs: Vec::new(),
            note: format!(
                "shadowed: {} run(s) would have been cancelled; the decisions are recorded \
                 and nothing was applied",
                targets.len()
            ),
        })
        .into_response());
    }

    // One settle window over the whole set: the drains tear down
    // concurrently, so the first run's wait overlaps the rest.
    let deadline = tokio::time::Instant::now() + CANCEL_SETTLE;
    let mut runs = Vec::with_capacity(issued.len());
    for (kind, id, decision_seq) in issued {
        let (concluded, status) = settle(&store, kind, &id, deadline).await?;
        let note = match concluded {
            true => format!("the {} stopped: it is now {status}", kind.noun()),
            false => format!(
                "the cancel is recorded and the {} is still {status}; whoever is \
                 following the run concludes it",
                kind.noun()
            ),
        };
        runs.push(CancelAck {
            run_kind: kind,
            run_id: id,
            concluded,
            status,
            decision_seq,
            note,
        });
    }
    let concluded = runs.iter().filter(|ack| ack.concluded).count();
    let note = format!(
        "{} run(s) asked to stop; {concluded} concluded before this answered",
        runs.len()
    );
    Ok(Json(CancelAllResponse { runs, note }).into_response())
}

/// Poll the run until `deadline`, and report where it got to.
///
/// Polling rather than waiting on the event stream because the run may be
/// concluded by a *different* process (one that reattached to it), whose
/// terminal write this server only sees in the database. The deadline is the
/// caller's so that a bulk cancel can settle its whole set inside one
/// [`CANCEL_SETTLE`] window — the teardowns run concurrently, so waiting a
/// fresh window per run would charge serially for work that isn't.
async fn settle(
    store: &Store,
    kind: RunKind,
    id: &str,
    deadline: tokio::time::Instant,
) -> ApiResult<(bool, String)> {
    loop {
        let (concluded, status) = match kind {
            RunKind::Session => {
                let session = store
                    .get_session(&SessionId::from_raw(id.to_string()))
                    .await?
                    .ok_or_else(|| ApiError::NotFound(format!("session {id}")))?;
                (
                    session.status != SessionStatus::Running,
                    session.status.as_str().to_string(),
                )
            }
            RunKind::Build => {
                let build = store
                    .get_build(&BuildId::from_raw(id.to_string()))
                    .await?
                    .ok_or_else(|| ApiError::NotFound(format!("build {id}")))?;
                (
                    build.status.is_terminal(),
                    build.status.as_str().to_string(),
                )
            }
        };
        if concluded || tokio::time::Instant::now() >= deadline {
            return Ok((concluded, status));
        }
        tokio::time::sleep(CANCEL_SETTLE_POLL).await;
    }
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

/// Who a write belongs to: the [`ACTOR_HEADER`] claim, or the human when no
/// claim is made.
///
/// A claim that is *present but does not verify* is a 403, not the human.
/// Since the human is never gated, demoting a failed claim would hand the
/// caller more authority than it asked for — the charter would go silently
/// unenforced, the ledger would misattribute the write, and the echo filter
/// would nudge the orchestrator about its own action. Failing closed costs a
/// turn; failing open costs all of that.
fn actor_of(store: &Store, headers: &axum::http::HeaderMap) -> ApiResult<Actor> {
    match store.resolve_actor(headers.get(ACTOR_HEADER).and_then(|v| v.to_str().ok())) {
        ActorClaim::Human => Ok(Actor::Human),
        ActorClaim::Orchestrator => Ok(Actor::Orchestrator),
        ActorClaim::Unrecognized => Err(ApiError::Forbidden(format!(
            "{ACTOR_HEADER} did not verify — expected `orchestrator <token>` with the \
             token this server minted at boot. A failed claim is refused rather than \
             read as the human; send no header at all to write as the human."
        ))),
    }
}

// --- the charter: what the orchestrator may do ---

/// What a caller is allowed to do with a capability, right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authority {
    /// Apply it. Every human write, and an orchestrator write on a `live`
    /// capability.
    Perform,
    /// Record the judgment, change nothing. The orchestrator on `shadow`.
    Shadow,
}

/// Decide whether this caller may take this action, and how.
///
/// The human is never gated: they are the accountable party, and a tool that
/// asks its owner for permission is just slower. For the orchestrator the
/// charter decides, and the check lives here rather than in the prompt on
/// purpose — prompt text is precisely what a restarted or overlong session
/// misweighs, and "authority the model can talk itself into" is not authority.
///
/// The daily cap is a mechanical floor, not a judgment: it bounds a runaway
/// loop, and it is checked only for actions that will actually be applied.
///
/// It takes the whole [`DecisionInput`] rather than just the actor because it
/// is also where the rationale is required. That rule used to live only inside
/// the store call that writes the ledger row, which on every enforced path
/// runs *after* the GitHub write it explains — so a rationale-less
/// `POST /issues` filed the issue and then 400'd, and every retry filed
/// another one with no ledger row behind it (#957). This is the one call every
/// gated handler already makes before its effect, so a handler nobody has
/// written yet inherits the ordering; a per-handler check does not, which is
/// what three of nine having one and six not having one demonstrated.
///
/// The order of the three refusals is deliberate. `Off` answers first, because
/// a rationale cannot rescue a capability that was never going to act, and
/// telling a caller to write one for a call that will 403 anyway sends it to
/// fix the wrong thing. The rationale answers before `Shadow`, because a
/// shadow row *is* recorded, and a recorded decision with an empty rationale
/// is exactly the unreviewable artifact this exists to prevent.
async fn authorize(
    store: &Store,
    decision: &DecisionInput,
    capability: Capability,
    action: DecisionAction,
) -> ApiResult<Authority> {
    if decision.actor == Actor::Human {
        return Ok(Authority::Perform);
    }
    let entry = store.charter_entry(capability).await?;
    if entry.level == CharterLevel::Off {
        return Err(ApiError::Forbidden(format!(
            "{} is off in the charter — say what you would do and why, and leave it to the human",
            capability.as_str()
        )));
    }
    // `StoreError::Invalid` maps to 400, so a rejected call gets the same
    // status and the same sentence it always did — only the side effect is
    // gone.
    require_rationale(decision)?;
    if entry.level == CharterLevel::Shadow {
        return Ok(Authority::Shadow);
    }
    if let Some(limit) = entry.daily_limit {
        let used = store.orchestrator_actions_today(action).await?;
        if used >= limit {
            return Err(ApiError::Forbidden(format!(
                "{} has used its {limit}/day budget ({used} so far) — \
                 it resets at midnight UTC",
                capability.as_str()
            )));
        }
    }
    Ok(Authority::Perform)
}

/// The charter as it stands. Readable by anyone; the orchestrator's own
/// authority section is generated from this same data every turn.
async fn get_charter(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<CharterEntry>>> {
    Ok(Json(store.charter().await?))
}

/// Change what the orchestrator may do. Human-only, and enforced as such:
/// a capability that could widen its own charter would not be a charter.
async fn set_charter(
    State(store): State<Arc<Store>>,
    Path(capability): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetCharter>,
) -> ApiResult<Json<CharterEntry>> {
    if actor_of(&store, &headers)? != Actor::Human {
        return Err(ApiError::Forbidden(
            "the charter is the human's to set".into(),
        ));
    }
    let capability = Capability::from_str(&capability)
        .ok_or_else(|| ApiError::NotFound(format!("unknown capability: {capability}")))?;
    let level = CharterLevel::from_str(&body.level)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown level: {}", body.level)))?;
    Ok(Json(
        store
            .set_charter(capability, level, body.daily_limit)
            .await?,
    ))
}

/// `GET /autonomy-notice` — has anyone ever been shown what unattended
/// operation means? (#993)
///
/// **Readable by anyone, deliberately.** A client that cannot read this cannot
/// decide whether to explain, and the failure it would fall into is explaining
/// on every press — which is how a notice stops being read.
///
/// **This is not a gate.** No handler in this file consults the row before
/// acting, and adding one would be a deliberate new reader rather than a
/// refactor.
async fn get_autonomy_notice(State(store): State<Arc<Store>>) -> ApiResult<Json<AutonomyNotice>> {
    Ok(Json(store.autonomy_notice().await?))
}

/// `POST /autonomy-notice/ack` — record that a person was shown it.
///
/// **Human-only, and not charter-gated**, on the `build-now` precedent: the row
/// records that a *person* was told, so an orchestrator able to write it would
/// be clicking through its own disclosure — and the record would then say a
/// human had been informed when none had. It is not a unit of work inside the
/// pipeline and none of the nine capabilities describes it.
///
/// Idempotent: the store keeps the first acknowledgement, so a client may fire
/// this without reading first and a second surface cannot move the timestamp.
async fn acknowledge_autonomy_notice(
    State(store): State<Arc<Store>>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<AutonomyNotice>> {
    if actor_of(&store, &headers)? != Actor::Human {
        return Err(ApiError::Forbidden(
            "acknowledging what unattended operation means is the human's alone: the row records \
             that a person was shown this, so an agent writing it would be clicking through its \
             own disclosure. Nothing is gated on it either way."
                .into(),
        ));
    }
    Ok(Json(store.acknowledge_autonomy_notice().await?))
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
    if q.pending.unwrap_or(false) {
        if subject.is_some() {
            return Err(ApiError::BadRequest(
                "pass a subject or ?pending=true, not both".into(),
            ));
        }
        return Ok(Json(store.pending_decisions().await?));
    }
    Ok(Json(
        store.decisions(subject, q.limit.unwrap_or(100)).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct DecisionQuery {
    spec: Option<String>,
    build: Option<String>,
    limit: Option<i64>,
    /// Every row whose effect nobody confirmed, oldest first. The working set
    /// for `ObligationKind::ReconcileDecision`.
    pending: Option<bool>,
}

/// Ask GitHub — with **this server's** credential — whether a pending
/// decision's artifact exists.
///
/// This is what makes `ObligationKind::ReconcileDecision` dischargeable, and
/// it is the reason the obligation exists at all. `GET /decisions?pending=true`
/// says a row is pending; it cannot say whether the artifact is upstream, and
/// the default `ORCHESTRATOR_CMD` is `--allowedTools Bash(curl:*)` with no
/// `GITHUB_TOKEN` of its own — so leaving the lookup to the recipient leaves
/// it a choice between guessing and doing nothing. A guess writes `applied` or
/// `annulled` into an append-only ledger on no evidence, which is worse than
/// the missing row this whole mechanism exists to prevent, and it is worse in
/// the specific way #964 warns about: a row claiming an effect makes every row
/// suspect. It also does not stop at the ledger — an orchestrator that cannot
/// tell whether its capture landed has one obvious move, which is to file it
/// again, and that is #957's second-issue failure one level up.
///
/// A read, so it settles nothing itself: it returns what it found, and
/// `POST /decisions/{seq}/settle` writes the answer down. `unknown` is a real
/// and common verdict — the row stays pending, which is the same honest
/// description it had before.
async fn reconcile_decision(
    State(store): State<Arc<Store>>,
    State(github): State<Option<Arc<GitHubClient>>>,
    Path(seq): Path<i64>,
) -> ApiResult<Json<DecisionReconciliation>> {
    let decision = store
        .decision(seq)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("decision {seq}")))?;
    if decision.state != DecisionState::Pending {
        return Err(ApiError::BadRequest(format!(
            "decision {seq} is {} — there is no open window to reconcile",
            decision.state.as_str()
        )));
    }
    let github = github.ok_or_else(|| {
        ApiError::Unavailable(
            "no GITHUB_TOKEN: this server cannot look the artifact up, and nothing else              should guess on its behalf"
                .into(),
        )
    })?;
    let intent = decision
        .outcome
        .as_ref()
        .and_then(|o| o.get("intent"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let repo = intent.get("repo").and_then(|r| r.as_str()).unwrap_or("");
    let (owner, name) = repo.split_once('/').unwrap_or(("", ""));
    if owner.is_empty() || name.is_empty() {
        return Ok(Json(DecisionReconciliation {
            seq,
            action: decision.action.as_str().to_string(),
            verdict: "unknown".into(),
            found: serde_json::Value::Null,
            note: "the intent does not name a repository, so there is nowhere to look —                    settle this one by hand or leave it pending"
                .into(),
        }));
    }
    let number = intent
        .get("number")
        .or_else(|| intent.get("issue"))
        .and_then(|n| n.as_u64());

    let looked_up = look_up_artifact(&github, &decision, &intent, owner, name, number).await;
    let (verdict, found, note) = match looked_up {
        Ok(answer) => answer,
        // A failed lookup is `unknown` and never `annulled`: "we could not
        // ask" and "it did not happen" are the two answers that must never be
        // confused here.
        Err(e) => (
            "unknown",
            serde_json::json!({ "lookup_failed": e.to_string() }),
            format!("asking GitHub failed ({e}); the row stays pending"),
        ),
    };
    Ok(Json(DecisionReconciliation {
        seq,
        action: decision.action.as_str().to_string(),
        verdict: verdict.to_string(),
        found,
        note,
    }))
}

/// The per-action lookup behind [`reconcile_decision`]: what artifact this
/// action would have produced, and is it there.
///
/// Exhaustive on [`DecisionAction`], so a new action has to say how it is
/// reconciled — or say that it has no upstream artifact, which is the honest
/// answer for every store-only decision and is why they never go pending.
async fn look_up_artifact(
    github: &GitHubClient,
    decision: &Decision,
    intent: &serde_json::Value,
    owner: &str,
    name: &str,
    number: Option<u64>,
) -> Result<(&'static str, serde_json::Value, String), crate::github::GhError> {
    let text = |key: &str| {
        intent
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Ok(match decision.action {
        DecisionAction::CaptureWork => {
            let title = text("title");
            let numbers = github.find_issues_by_title(owner, name, &title).await?;
            if numbers.is_empty() {
                (
                    "annulled",
                    serde_json::json!({ "matching_issues": [] }),
                    format!("no issue in {owner}/{name} carries the title that was filed"),
                )
            } else {
                (
                    "applied",
                    serde_json::json!({ "matching_issues": numbers }),
                    format!("the issue exists upstream as #{}", numbers[0]),
                )
            }
        }
        DecisionAction::RetireWork | DecisionAction::ReopenWork => {
            let Some(number) = number else {
                return Ok(unaddressed());
            };
            let facts = github.issue_facts(owner, name, number).await?;
            let want = if decision.action == DecisionAction::RetireWork {
                GhState::Closed
            } else {
                GhState::Open
            };
            let verdict = if facts.state == want { "applied" } else { "annulled" };
            (
                verdict,
                serde_json::json!({ "issue": number, "state": facts.state.as_str() }),
                format!("#{number} is {} upstream", facts.state.as_str()),
            )
        }
        DecisionAction::EditIssue => {
            let Some(number) = number else {
                return Ok(unaddressed());
            };
            let facts = github.issue_facts(owner, name, number).await?;
            // Only the fields the edit actually asked to change are compared:
            // an edit that set only the body says nothing about the title.
            let title_ok = intent
                .get("title")
                .and_then(|t| t.as_str())
                .is_none_or(|t| t == facts.title);
            let body_ok = intent
                .get("body")
                .and_then(|b| b.as_str())
                .is_none_or(|b| b == facts.body);
            let verdict = if title_ok && body_ok { "applied" } else { "annulled" };
            (
                verdict,
                serde_json::json!({ "issue": number, "title": facts.title, "body": facts.body }),
                format!("#{number}'s current text {} what the edit asked for",
                        if title_ok && body_ok { "is" } else { "is not" }),
            )
        }
        DecisionAction::LabelIssue => {
            let Some(number) = number else {
                return Ok(unaddressed());
            };
            let facts = github.issue_facts(owner, name, number).await?;
            let wanted: Vec<String> = intent
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|l| {
                    l.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut have = facts.labels.clone();
            let mut want = wanted.clone();
            have.sort();
            want.sort();
            let verdict = if have == want { "applied" } else { "annulled" };
            (
                verdict,
                serde_json::json!({ "issue": number, "labels": facts.labels }),
                format!("#{number} carries {} labels", facts.labels.len()),
            )
        }
        DecisionAction::CommentOnWork | DecisionAction::ReviewComment => {
            let Some(number) = number else {
                return Ok(unaddressed());
            };
            let wanted = text("body");
            let bodies = if decision.action == DecisionAction::CommentOnWork {
                github.list_issue_comments(owner, name, number).await?
            } else {
                github.list_review_comments(owner, name, number).await?
            };
            let posted = bodies.iter().any(|b| b.trim() == wanted.trim());
            (
                if posted { "applied" } else { "annulled" },
                serde_json::json!({ "number": number, "comments": bodies.len(), "found": posted }),
                format!(
                    "the comment {} among the {} on #{number}",
                    if posted { "is" } else { "is not" },
                    bodies.len()
                ),
            )
        }
        DecisionAction::MergeBuild | DecisionAction::AbandonBuild => {
            let Some(number) = number else {
                return Ok(unaddressed());
            };
            let pr = github.pull_request_state(owner, name, number).await?;
            let done = if decision.action == DecisionAction::MergeBuild {
                pr.merged
            } else {
                pr.state == GhState::Closed && !pr.merged
            };
            (
                if done { "applied" } else { "annulled" },
                serde_json::json!({
                    "number": number,
                    "state": pr.state.as_str(),
                    "merged": pr.merged,
                }),
                format!(
                    "PR #{number} is {} and merged = {}",
                    pr.state.as_str(),
                    pr.merged
                ),
            )
        }
        // No upstream artifact: these commit in the same transaction as the
        // state they authorize, so they are never written pending and nothing
        // should be here asking about one.
        DecisionAction::Approve
        | DecisionAction::NeedsRevision
        | DecisionAction::Reject
        | DecisionAction::RequestBuild
        | DecisionAction::AuthorSpec
        | DecisionAction::QueueTask
        | DecisionAction::CancelRun
        | DecisionAction::SettleDecision => (
            "unknown",
            serde_json::Value::Null,
            "this action never reaches another system, so it has no artifact to find —              a pending row here is a bug, not a window"
                .into(),
        ),
    })
}

fn unaddressed() -> (&'static str, serde_json::Value, String) {
    (
        "unknown",
        serde_json::Value::Null,
        "the intent names no issue or pull request number, so there is nowhere to look".into(),
    )
}

/// Write down what became of a pending decision.
///
/// **Never charter-gated, deliberately.** The obvious design borrows the
/// settled row's authority via [`DecisionAction::capability`], and it has a
/// failure the charter's own purpose creates: `shadow`/`off` exist for
/// *demotion*, and demotion is most likely exactly when something has gone
/// wrong, which is when pending rows exist. Demote `capture_work` with a
/// capture pending and the recipient would be raised an obligation every
/// thirty minutes that the server refuses forever.
///
/// So: settling is not the action. The effect already happened, and refusing
/// to record it does not un-file the issue — it only keeps the ledger wrong.
/// Recording an outcome exercises no authority over anything outside this
/// database, which is why a demoted capability does not gate it. The
/// capability the settled row came from is still reported, on the row and in
/// the brief, so a reader can see that the thing being settled is one the
/// charter has since switched off.
///
/// What is still required is a rationale from the orchestrator, on the
/// ordinary rule: an unexplained autonomous row is unreviewable, and this one
/// is a claim about the world.
async fn settle_decision(
    State(store): State<Arc<Store>>,
    Path(seq): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SettleDecisionRequest>,
) -> ApiResult<Response> {
    let state = DecisionState::from_str(&body.state).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "unknown state: {} (applied or annulled)",
            body.state
        ))
    })?;
    if !state.is_terminal() {
        return Err(ApiError::BadRequest(
            "a decision settles to applied or annulled; pending is where it already is".into(),
        ));
    }
    let actor = actor_of(&store, &headers)?;
    let existing = store
        .decision(seq)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("decision {seq}")))?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    let settle_seq = store
        .reconcile_decision(seq, state, body.outcome.as_ref(), &decision)
        .await?;
    Ok(Json(serde_json::json!({
        "settled": seq,
        "state": state.as_str(),
        "action": existing.action.as_str(),
        "capability": existing.action.capability().map(|c| c.as_str()),
        "decision_seq": settle_seq,
    }))
    .into_response())
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
) -> ApiResult<Response> {
    let status = SpecQueueStatus::from_str(&body.status)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown status: {}", body.status)))?;
    let id = SpecId::from_raw(spec_id);
    let actor = actor_of(&store, &headers)?;
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    let action = review_action(status)?;
    if authorize(&store, &decision, Capability::AutoReviewSpecs, action).await? == Authority::Shadow
    {
        // A shadow verdict still discharges the review obligation: the
        // orchestrator has done everything it is allowed to do, and
        // re-reminding it forever would be nagging about work it cannot
        // finish. What remains is the human's turn.
        let seq = store
            .record_decision("spec", id.as_str(), action, decision, false)
            .await?;
        return Ok(shadowed(seq, "the spec's status is unchanged"));
    }
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
    })
    .into_response())
}

/// The ledger action a verdict corresponds to. Rejects the statuses that are
/// not verdicts before anything is recorded.
fn review_action(status: SpecQueueStatus) -> ApiResult<DecisionAction> {
    match status {
        SpecQueueStatus::Approved => Ok(DecisionAction::Approve),
        SpecQueueStatus::Rejected => Ok(DecisionAction::Reject),
        SpecQueueStatus::NeedsRevision => Ok(DecisionAction::NeedsRevision),
        other => Err(ApiError::BadRequest(format!(
            "{} is not a review outcome",
            other.as_str()
        ))),
    }
}

// --- builds ---

/// 202, not 200/201-with-result: builds are serial and this only queues one.
/// Watch `build_started` / `build_completed` on the event stream, or poll
/// `GET /builds/{id}`.
async fn request_build(
    State(store): State<Arc<Store>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<BuildRequest>,
) -> ApiResult<Response> {
    let base_branch = body.base_branch.as_deref().unwrap_or("main");
    let actor = actor_of(&store, &headers)?;
    // A build has no staging step — it is created for one run — so "absent"
    // and "cleared" are the same answer here and the outer Option flattens.
    let directions = parse_directions(body.directions.as_deref(), actor)?.flatten();
    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    if authorize(
        &store,
        &decision,
        Capability::DispatchBuilds,
        DecisionAction::RequestBuild,
    )
    .await?
        == Authority::Shadow
    {
        let subject = body
            .spec_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let seq = store
            .record_decision(
                "build",
                &subject,
                DecisionAction::RequestBuild,
                decision,
                false,
            )
            .await?;
        return Ok(shadowed(seq, "no Builder run was queued"));
    }
    let build = store
        .create_directed_build(&body.spec_ids, base_branch, directions.as_ref(), decision)
        .await?;
    let spec_ids = store.build_spec_ids(&build.id).await?;
    Ok((StatusCode::ACCEPTED, Json(BuildDetail { build, spec_ids })).into_response())
}

/// `POST /tasks/{task_id}/build-now` — skip the Scout for a task whose issue
/// body already is the specification (#869).
///
/// One call, because from the human's side it is one decision: write the spec,
/// approve it, queue the build. 202 with the same [`BuildDetail`] as
/// `POST /builds`, and for the same reason — builds are serial, so this queues
/// one rather than starting it.
///
/// **Human-only, and not charter-gated.** The orchestrator is refused outright
/// rather than checked against `dispatch_builds`: authoring a spec, approving
/// it, and dispatching a build off it with no second opinion anywhere in the
/// loop is a materially different autonomy from batching specs a reviewer
/// already ruled on. If it is ever granted it wants its own named capability,
/// which is a decision for a human and an issue, not for this handler.
///
/// The two store calls are deliberately not merged into one transaction. If
/// `create_build` fails the spec is still approved and the task sits in
/// `ready_to_build`, recoverable with a plain `POST /builds` — a better place
/// to land than silently discarding what the human wrote.
async fn build_task_now(
    State(store): State<Arc<Store>>,
    Path(task_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<BuildNowRequest>>,
) -> ApiResult<Response> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let actor = actor_of(&store, &headers)?;
    if actor != Actor::Human {
        return Err(ApiError::Forbidden(
            "build-now is the human's alone: it writes a spec, approves it, and dispatches \
             a build in one act, with no second opinion anywhere in the loop. No charter \
             capability covers that. Propose it to the human, or send the task to a Scout \
             with POST /tasks/{id}/scout and review the spec it writes."
                .into(),
        ));
    }

    let id = TaskId::from_raw(task_id);
    let task = store
        .get_task(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("task {id}")))?;

    let complexity = match body.complexity.as_deref() {
        // A task worth skipping the Scout for is one nobody needed to explore.
        None => Complexity::Simple,
        Some(raw) => Complexity::from_str(raw)
            .ok_or_else(|| ApiError::BadRequest(format!("unknown complexity: {raw}")))?,
    };
    // A supplied `content` *replaces* the issue body rather than extending it:
    // the Builder prompt is spec content alone, so whatever lands here is the
    // whole of what the Builder reads.
    let content = body.content.unwrap_or_else(|| task.body.clone());
    if content.trim().is_empty() {
        return Err(ApiError::BadRequest(format!(
            "issue #{} has an empty body and no content was supplied — there would be \
             nothing for the Builder to implement",
            task.gh_issue_number
        )));
    }

    // Strictly beside `content`, never inside it: the spec is the artifact a
    // reviewer would read, and an instruction addressed to the agent does not
    // belong in it.
    let directions = parse_directions(body.directions.as_deref(), actor)?.flatten();

    let decision = DecisionInput {
        actor,
        rationale: body.rationale,
        evidence: body.evidence,
    };
    let spec = store
        .author_spec(&id, &content, complexity, decision.clone())
        .await?;
    let build = store
        .create_directed_build(
            std::slice::from_ref(&spec.id),
            body.base_branch.as_deref().unwrap_or("main"),
            directions.as_ref(),
            decision,
        )
        .await?;
    let spec_ids = store.build_spec_ids(&build.id).await?;
    Ok((StatusCode::ACCEPTED, Json(BuildDetail { build, spec_ids })).into_response())
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

// --- preserved bundles ---

/// The bundle service, or a 503 that says what is missing.
///
/// Never an empty list: a server with no bundle service has not looked in the
/// directory, and answering `[]` would say it looked and found nothing. That
/// is the one wrong answer to give about work that exists in exactly one
/// place.
fn bundle_service(bundles: &Option<Arc<RejectedBundles>>) -> ApiResult<&Arc<RejectedBundles>> {
    bundles.as_ref().ok_or_else(|| {
        ApiError::Unavailable(
            "this server has no bundle directory configured, so it cannot say what was \
             preserved — which is not the same as nothing having been"
                .into(),
        )
    })
}

/// Everything a client needs about one preserved bundle, joined from the file
/// and the build row at request time. Nothing here is stored.
async fn describe_bundle(
    store: &Store,
    file: crate::bundles::BundleFile,
) -> ApiResult<Option<RejectedBundle>> {
    // A bundle whose build row is gone — a wiped database, a file copied in
    // by hand — has no branch and no base, so there is no honest recovery
    // command to print. Reported as absent rather than guessed at; the file
    // stays on disk, which is the safe direction.
    let Some(build) = store.get_build(&file.build_id).await? else {
        return Ok(None);
    };
    let task_ids = store.build_task_ids(&file.build_id).await?;
    let superseded = store.build_superseded(&file.build_id).await?;
    Ok(Some(RejectedBundle {
        recovery_command: crate::bundles::recovery_command(&file.path, &build.branch),
        build_id: file.build_id,
        path: file.path.display().to_string(),
        bytes: file.bytes,
        created_at: file.created_at,
        branch: build.branch,
        base_sha: build.base_sha,
        head_sha: build.head_sha,
        exit_reason: build.exit_reason,
        task_ids,
        superseded,
    }))
}

/// `GET /bundles` — every implementation whose branch could not be pushed,
/// newest first. An empty list is the ordinary answer.
async fn list_bundles(
    State(store): State<Arc<Store>>,
    State(bundles): State<Option<Arc<RejectedBundles>>>,
) -> ApiResult<Json<Vec<RejectedBundle>>> {
    let bundles = bundle_service(&bundles)?;
    let files = bundles
        .list()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        match describe_bundle(&store, file).await? {
            Some(bundle) => out.push(bundle),
            None => continue,
        }
    }
    Ok(Json(out))
}

/// `GET /builds/{build_id}/bundle` — 404 when there is none, which is the
/// ordinary case for every build that landed its branch. Same shape as
/// `GET /sessions/{id}/notes`.
async fn get_build_bundle(
    State(store): State<Arc<Store>>,
    State(bundles): State<Option<Arc<RejectedBundles>>>,
    Path(build_id): Path<String>,
) -> ApiResult<Json<RejectedBundle>> {
    let bundles = bundle_service(&bundles)?;
    let id = BuildId::from_raw(build_id);
    let missing = || ApiError::NotFound(format!("no preserved bundle for build {id}"));
    let file = bundles
        .stat(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(missing)?;
    describe_bundle(&store, file)
        .await?
        .map(Json)
        .ok_or_else(missing)
}

/// `DELETE /builds/{build_id}/bundle` — throw an implementation away.
///
/// **Human-only, and refused to the orchestrator outright** rather than
/// charter-gated. The retention policy ([`crate::run::reclaim_bundles`])
/// already deletes everything that has demonstrably been reproduced and
/// shipped; what is left is by definition work that exists in exactly one
/// place, and deciding it is not worth keeping is a judgment with no recourse
/// afterwards. There is no capability that covers that today, and if one is
/// ever wanted it should be named and argued for rather than folded into
/// `dispatch_builds`.
///
/// 204 on success, 404 when there was nothing there — so a second click, or a
/// reclaim that got there first, is honest rather than an error.
async fn delete_build_bundle(
    State(store): State<Arc<Store>>,
    State(bundles): State<Option<Arc<RejectedBundles>>>,
    Path(build_id): Path<String>,
    headers: axum::http::HeaderMap,
) -> ApiResult<StatusCode> {
    let bundles = bundle_service(&bundles)?;
    let actor = actor_of(&store, &headers)?;
    if actor != Actor::Human {
        return Err(ApiError::Forbidden(
            "deleting a preserved bundle is the human's alone: it is the only copy of an \
             implementation, and nothing recovers it afterwards. Work that has been \
             reproduced and shipped is reclaimed by the retention policy without anyone \
             asking. Say which bundle you think is redundant and why."
                .into(),
        ));
    }
    let id = BuildId::from_raw(build_id);
    // Read before the delete: after it, nothing can say whether this was
    // bookkeeping or a loss, and that distinction is the whole of what the
    // event is for.
    let superseded = store.build_superseded(&id).await.unwrap_or(false);
    if !bundles
        .remove(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        return Err(ApiError::NotFound(format!(
            "no preserved bundle for build {id}"
        )));
    }
    warn!(build_id = %id, superseded, "a preserved bundle was deleted by hand");
    store
        .append_event(EventPayload::BundleRemoved {
            build_id: id,
            superseded,
            actor,
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- orchestrator ---

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    /// Incremental catch-up: turns after this seq, oldest first.
    since: Option<i64>,
    /// Page backwards: the turns immediately before this seq.
    before: Option<i64>,
    /// Page size, clamped to [`MESSAGE_PAGE_MAX`].
    limit: Option<i64>,
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

/// The conversation, always bounded.
///
/// `?since=` catches a client up, `?before=` pages backwards, and neither
/// opens on the newest window. The history is kept forever — it is the read
/// that is capped, because the app refetches on every event and an unbounded
/// one made bytes grow as messages x events.
async fn list_orchestrator_messages(
    State(store): State<Arc<Store>>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult<Json<Vec<OrchestratorMessage>>> {
    if q.since.is_some() && q.before.is_some() {
        return Err(ApiError::BadRequest(
            "pass since or before, not both".into(),
        ));
    }
    let limit = q
        .limit
        .unwrap_or(MESSAGE_PAGE_DEFAULT)
        .clamp(1, MESSAGE_PAGE_MAX);
    let messages = match q.since {
        Some(since) => store.orchestrator_messages_since(since, limit).await?,
        None => store.orchestrator_messages_window(q.before, limit).await?,
    };
    Ok(Json(messages))
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

// --- viewer ---

/// `GET /viewer` — who the server's own GitHub credential is.
///
/// **Its own route rather than a `/status` field.** `/status` is `reload`'s
/// liveness probe for a swap and is polled by `tasks status` and the Server
/// window, and its own `pool` field already carries the rule that a status
/// request must not make a round trip to another daemon. A GitHub call there
/// would be the same mistake with a longer tail.
///
/// **Always a 200.** All three answers are states this route reports, not
/// failures of it: a fresh machine with no token is the *common* case, and a
/// 503 there would put a red banner on an app that is working correctly.
///
/// Not charter-gated and it writes no `decisions` row — it is a read that
/// changes nothing upstream.
///
/// It is the second route that spends the server's GitHub credential outbound
/// on a `GET`, so it joins `GET /decisions/{seq}/reconcile` in
/// [`crate::loopback`]'s stated residual — a cross-site `<img src>` carries no
/// `Origin` and a loopback `Host`, so it passes the guard. What bounds it is
/// the cache this route needed anyway: a forced read costs at most one GitHub
/// call per failure TTL however often it is triggered.
async fn get_viewer(
    State(cache): State<Arc<crate::viewer::ViewerCache>>,
    State(github): State<Option<Arc<GitHubClient>>>,
) -> Json<Viewer> {
    Json(cache.get(github.as_ref()).await)
}

// --- mode ---

/// `GET /status` — who is serving, since when, what this boot migrated, and
/// what is in flight. See [`ServerStatus`]: a 200 here is the claim that
/// *this* pid opened the database and finished its migrations, which is what
/// makes it a usable liveness probe for `tasks reload`.
async fn get_status(
    State(store): State<Arc<Store>>,
    State(github_health): State<Option<Arc<GitHubHealth>>>,
    State(updates): State<Option<Arc<crate::updates::UpdateWatch>>>,
    State(pool_health): State<Option<Arc<crate::pool_health::PoolHealth>>>,
) -> ApiResult<Json<ServerStatus>> {
    // Through the same watch the dispatchers consult, so `/status` cannot
    // claim a hold they are not honouring.
    let update = match updates {
        Some(watch) => watch.pending(&store).await,
        None => None,
    };
    Ok(Json(ServerStatus {
        pid: std::process::id(),
        started_at: serving_since(),
        migrations_applied: store.migrations_applied().to_vec(),
        mode: store.get_mode().await?,
        in_flight: store.in_flight().await?,
        // Judged against *this* binary's build, here at read time — see
        // `Store::image_builds`.
        images: store.image_builds(crate::version::VERSION).await?,
        // Through the same `hold` predicate the two dispatchers use, so
        // `/status` cannot claim a hold they are not honouring — that is the
        // whole reason the staleness window is bound at construction rather
        // than at each read.
        github: github_health
            .and_then(|health| health.hold(Utc::now()))
            .map(|outage| GitHubHold {
                since: outage.since,
                last_seen: outage.last,
                failures: outage.failures,
                error: outage.error,
            }),
        update,
        // The third hold, read through the same predicate the gates use. It is
        // deliberately *not* probed here: a status request must not make a
        // round trip to another daemon, and the gates refresh the record every
        // few seconds anyway — the staleness window is what keeps this honest
        // if they stop.
        pool: pool_health
            .and_then(|health| health.hold(Utc::now()))
            .map(|run| run.to_hold()),
    }))
}

async fn get_mode(State(store): State<Arc<Store>>) -> ApiResult<Json<ModeResponse>> {
    Ok(Json(ModeResponse {
        mode: store.get_mode().await?,
    }))
}

/// `POST /mode` — set the mode, and say why if the caller has a reason.
///
/// The `ModeChanged` event is conditional on the mode actually moving; the
/// `note` is not. A maintenance drain against an already-paused pipeline
/// changes nothing and is still the fact somebody arriving later needs — and
/// a `Note` is not `nudge_worthy`, so it costs no orchestrator turn either
/// way.
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
    if let Some(note) = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        store
            .append_event(EventPayload::Note {
                source: MODE_SOURCE.into(),
                message: note.to_string(),
            })
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

/// Catch-up read of a scout session's agent output. `since` is **inclusive**,
/// to match `/events?since=` — a tailing client passes `last_seq + 1`. An empty
/// array means "nothing recorded", not an error: sessions that predate
/// transcript capture have none.
async fn list_transcript(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> ApiResult<Json<Vec<TranscriptLine>>> {
    let owner = session_owner(&store, session_id).await?;
    list_owner_transcript(&store, &owner, query).await
}

/// The same read for a build. Builds get transcripts on identical terms —
/// same caps, same truncation marker, same inclusive `since` — because a
/// build that ran its whole budget and committed nothing is precisely the
/// thing nobody could diagnose before.
async fn list_build_transcript(
    State(store): State<Arc<Store>>,
    Path(build_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> ApiResult<Json<Vec<TranscriptLine>>> {
    let owner = build_owner(&store, build_id).await?;
    list_owner_transcript(&store, &owner, query).await
}

/// SSE tail of a session's transcript: replay from `since`, then live lines.
async fn stream_transcript(
    State(store): State<Arc<Store>>,
    Path(session_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>> + use<>>> {
    let owner = session_owner(&store, session_id).await?;
    stream_owner_transcript(&store, owner, query).await
}

/// SSE tail of a build's transcript, on the same contract.
async fn stream_build_transcript(
    State(store): State<Arc<Store>>,
    Path(build_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>> + use<>>> {
    let owner = build_owner(&store, build_id).await?;
    stream_owner_transcript(&store, owner, query).await
}

/// Resolve a path id to a transcript owner, 404ing on an unknown row — the
/// only difference between the two pairs of handlers above.
async fn session_owner(store: &Store, raw: String) -> ApiResult<TranscriptOwner> {
    let session_id = SessionId::from_raw(raw);
    if store.get_session(&session_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("session {session_id}")));
    }
    Ok(TranscriptOwner::Session { session_id })
}

async fn build_owner(store: &Store, raw: String) -> ApiResult<TranscriptOwner> {
    let build_id = BuildId::from_raw(raw);
    if store.get_build(&build_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("build {build_id}")));
    }
    Ok(TranscriptOwner::Build { build_id })
}

async fn list_owner_transcript(
    store: &Store,
    owner: &TranscriptOwner,
    query: TranscriptQuery,
) -> ApiResult<Json<Vec<TranscriptLine>>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TRANSCRIPT_LIMIT)
        .min(MAX_TRANSCRIPT_LIMIT);
    if limit <= 0 {
        return Err(ApiError::BadRequest("limit must be positive".into()));
    }
    let lines = store
        .transcript_since(owner, query.since.unwrap_or(0), limit)
        .await?;
    Ok(Json(lines))
}

/// Replay from `since`, then live lines.
///
/// The replay pages until it catches up and deliberately takes no `limit`. The
/// obvious alternative — one limit-sized page, then attach the tail — hands the
/// client a stream that jumps silently from the end of the page to the newest
/// line, and a stream is exactly the shape in which that hole goes unnoticed.
/// The per-run byte cap is what keeps reading it all affordable.
///
/// `use<>` on the return type is load-bearing: under Rust 2024 capture rules
/// the returned `impl Stream` would otherwise capture the `&Store` borrow, and
/// the handlers above wouldn't compile.
async fn stream_owner_transcript(
    store: &Store,
    owner: TranscriptOwner,
    query: TranscriptQuery,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>> + use<>>> {
    // Subscribe *before* reading history, so a line appended between the two
    // steps arrives on the live channel instead of falling down the gap.
    let live = store.subscribe_transcript();

    let mut backfill = Vec::new();
    let mut next = query.since.unwrap_or(0);
    loop {
        let page = store
            .transcript_since(&owner, next, MAX_TRANSCRIPT_LIMIT)
            .await?;
        let Some(last) = page.last() else { break };
        next = last.seq + 1;
        let full = page.len() as i64 == MAX_TRANSCRIPT_LIMIT;
        backfill.extend(page);
        if !full {
            break;
        }
    }

    let tail = BroadcastStream::new(live).filter_map(move |result| {
        let line = match result {
            Ok(line) => line,
            Err(err) => {
                warn!(error = %err, "transcript sse subscriber lagged");
                return None;
            }
        };
        // Drop other runs, and anything the backfill already delivered. The
        // whole owner is compared, not just its id: seq restarts per owner,
        // so an id match alone would let a build's line into a session's tail
        // if the two ever shared a raw id.
        if line.owner != owner || line.seq < next {
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
            status: ProjectStatus::Active,
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
            scout_directions: None,
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
            directions: None,
        };
        store.insert_session(&session).await.unwrap();
        let spec = Spec {
            id: SpecId::new(),
            session_id: Some(session.id.clone()),
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

    /// A project is born `active` — the column backfills every repo to what it
    /// already was, and a client that predates it reads the same thing.
    #[tokio::test]
    async fn a_new_project_is_active() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let created: Project = http
            .post(format!("{base}/projects"))
            .json(&json!({"repo_owner": "iamnbutler", "repo_name": "tasks"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(created.status, ProjectStatus::Active);
        assert_eq!(created.slug(), "iamnbutler/tasks");
    }

    /// `UNIQUE(repo_owner, repo_name)` is case-sensitive, so the duplicate
    /// check cannot be left to it: two rows for one repo would cost
    /// `resolve_project` its answer and have the poller ingest every issue
    /// twice.
    #[tokio::test]
    async fn adding_the_same_repo_in_a_different_case_is_refused() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        http.post(format!("{base}/projects"))
            .json(&json!({"repo_owner": "iamnbutler", "repo_name": "tasks"}))
            .send()
            .await
            .unwrap();
        let resp = http
            .post(format!("{base}/projects"))
            // Different case, and the stray whitespace/slash a paste carries.
            .json(&json!({"repo_owner": " IamNButler ", "repo_name": "Tasks/"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: ErrorResponse = resp.json().await.unwrap();
        assert!(
            body.error.contains("already tracked"),
            "says what is wrong: {}",
            body.error
        );
        assert_eq!(store.list_projects().await.unwrap().len(), 1);
    }

    /// Normalizing on the way in, not just on the way to the duplicate check:
    /// what is stored is what every clone URL and every list is built from.
    #[tokio::test]
    async fn a_pasted_repo_is_stored_trimmed() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store.clone()).await;

        let created: Project = reqwest::Client::new()
            .post(format!("{base}/projects"))
            .json(&json!({"repo_owner": " iamnbutler ", "repo_name": "tasks/"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(created.slug(), "iamnbutler/tasks");
        assert_eq!(
            store
                .get_project(&created.id)
                .await
                .unwrap()
                .unwrap()
                .slug(),
            "iamnbutler/tasks"
        );
    }

    #[tokio::test]
    async fn project_status_moves_and_names_itself_on_the_event_log() {
        let (store, project) = store_with_project().await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let paused: Project = http
            .post(format!("{base}/projects/{}/status", project.id))
            .json(&json!({"status": "paused"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(paused.status, ProjectStatus::Paused);
        assert!(!paused.status.dispatches());
        assert!(paused.status.ingests(), "a paused repo is still polled");

        let archived: Project = http
            .post(format!("{base}/projects/{}/status", project.id))
            .json(&json!({"status": "archived"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!archived.status.ingests());

        // Back again: archiving is the only removal there is, so it has to be
        // undoable.
        let active: Project = http
            .post(format!("{base}/projects/{}/status", project.id))
            .json(&json!({"status": "active"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(active.status, ProjectStatus::Active);

        let statuses: Vec<ProjectStatus> = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                EventPayload::ProjectStatusChanged { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![
                ProjectStatus::Paused,
                ProjectStatus::Archived,
                ProjectStatus::Active
            ],
            "the status is in the payload so a client can narrate the change \
             without refetching /projects"
        );
    }

    #[tokio::test]
    async fn an_unknown_project_status_names_the_legal_ones() {
        let (store, project) = store_with_project().await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/projects/{}/status", project.id))
            .json(&json!({"status": "pasued"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: ErrorResponse = resp.json().await.unwrap();
        assert!(
            body.error.contains("active")
                && body.error.contains("paused")
                && body.error.contains("archived"),
            "names the three legal ones rather than serde's variant list: {}",
            body.error
        );
    }

    /// Human-only, and *not* charter-gated: every capability is `live` and
    /// both writes are still 403. What repositories the pipeline is pointed at
    /// is not a unit of work inside it.
    #[tokio::test]
    async fn project_writes_refuse_the_orchestrator_however_wide_its_charter() {
        let (store, project) = store_with_project().await;
        for capability in [
            Capability::CaptureWork,
            Capability::CurateWork,
            Capability::QueueTasks,
            Capability::DispatchBuilds,
        ] {
            store
                .set_charter(capability, CharterLevel::Live, None)
                .await
                .unwrap();
        }
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();
        let claim = format!("orchestrator {}", store.actor_token().expose());

        let resp = http
            .post(format!("{base}/projects"))
            .header(ACTOR_HEADER, &claim)
            .json(&json!({"repo_owner": "someone", "repo_name": "else"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        assert_eq!(store.list_projects().await.unwrap().len(), 1);

        let resp = http
            .post(format!("{base}/projects/{}/status", project.id))
            .header(ACTOR_HEADER, &claim)
            .json(&json!({"status": "archived"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        assert_eq!(
            store
                .get_project(&project.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ProjectStatus::Active
        );
    }

    /// Archiving one of two repos must not break `POST /issues` for the one
    /// that is left: a 400 saying "2 projects configured" about a server with
    /// one live repo reads as a bug.
    #[tokio::test]
    async fn an_archived_project_does_not_count_towards_ambiguity() {
        let (store, live) = store_with_project().await;
        let archived = Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "old".into(),
            added_at: Utc::now(),
            status: ProjectStatus::Active,
        };
        store.insert_project(&archived).await.unwrap();

        assert!(
            resolve_project(&store, None).await.is_err(),
            "two live projects is genuinely ambiguous"
        );

        store
            .set_project_status(&archived.id, ProjectStatus::Archived)
            .await
            .unwrap();
        let resolved = resolve_project(&store, None).await.unwrap();
        assert_eq!(resolved.id, live.id);

        // Naming it explicitly still resolves it: commenting on its open PR
        // and closing its issue are exactly the work archiving does not
        // abandon.
        let named = resolve_project(&store, Some(archived.id.clone()))
            .await
            .unwrap();
        assert_eq!(named.id, archived.id);
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

    // --- directions (#917) ---

    /// Staging is three-way, and "absent" is the case that has to be right:
    /// a second `POST /scout` with no body must not unaim a run somebody
    /// already aimed.
    #[tokio::test]
    async fn scout_directions_omit_keeps_and_empty_clears() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 7, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        // No body at all — the shape every caller used before this existed.
        let resp = http
            .post(format!("{base}/tasks/{}/queue", task.id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "an empty POST still queues the task");
        let queued: Task = resp.json().await.unwrap();
        assert_eq!(queued.state, TaskState::Queued);
        assert_eq!(queued.scout_directions, None);

        let aimed: Task = http
            .post(format!("{base}/tasks/{}/scout", task.id))
            .json(&json!({"directions": "  start from the poller  "}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let staged = aimed.scout_directions.expect("staged");
        assert_eq!(staged.text, "start from the poller", "trimmed, not raw");
        assert_eq!(staged.author, Actor::Human);

        // Absent leaves it alone.
        let again: Task = http
            .post(format!("{base}/tasks/{}/scout", task.id))
            .json(&json!({"rationale": "bumping it"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            again.scout_directions.as_ref().map(|d| d.text.as_str()),
            Some("start from the poller"),
            "omitting the field must not unaim the run"
        );

        // Empty clears it.
        let cleared: Task = http
            .post(format!("{base}/tasks/{}/scout", task.id))
            .json(&json!({"directions": "   "}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cleared.scout_directions, None);
        assert_eq!(
            store
                .get_task(&task.id)
                .await
                .unwrap()
                .unwrap()
                .scout_directions,
            None
        );
    }

    /// Over the limit is a refusal, and it costs the request everything: no
    /// staging, and no queueing either. Truncating instead would hand a Scout
    /// half an instruction the caller has no way to know was cut.
    #[tokio::test]
    async fn oversized_directions_400_and_stage_nothing_and_queue_nothing() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 7, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let huge = "x".repeat(MAX_DIRECTIONS_BYTES + 1);
        let resp = http
            .post(format!("{base}/tasks/{}/queue", task.id))
            .json(&json!({"directions": huge}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let after = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(after.scout_directions, None, "nothing was staged");
        assert_eq!(
            after.state,
            TaskState::Backlog,
            "and the task was not queued either"
        );
    }

    /// The two fields are not interchangeable, in either direction. A
    /// `rationale` on `POST /builds` explains the batch to a later reader and
    /// must never reach the VM.
    #[tokio::test]
    async fn a_rationale_on_a_build_never_becomes_directions() {
        let (store, project) = store_with_project().await;
        let (_task, spec) = insert_spec(&store, &project, 7).await;
        store
            .review_spec(
                &spec.id,
                SpecQueueStatus::Approved,
                None,
                DecisionInput {
                    actor: Actor::Human,
                    rationale: None,
                    evidence: None,
                },
            )
            .await
            .unwrap();
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let detail: BuildDetail = http
            .post(format!("{base}/builds"))
            .json(&json!({
                "spec_ids": [spec.id.as_str()],
                "rationale": "this is the only approved spec",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            detail.build.directions, None,
            "a rationale is not an instruction to the agent"
        );

        let decisions = store
            .decisions(Some(("build", detail.build.id.as_str())), 10)
            .await
            .unwrap();
        assert_eq!(
            decisions[0].rationale.as_deref(),
            Some("this is the only approved spec")
        );
    }

    /// And the reverse: directions reach the build row, attributed, without
    /// touching the rationale.
    #[tokio::test]
    async fn build_directions_are_stored_with_their_author() {
        let (store, project) = store_with_project().await;
        let (_task, spec) = insert_spec(&store, &project, 7).await;
        store
            .review_spec(
                &spec.id,
                SpecQueueStatus::Approved,
                None,
                DecisionInput {
                    actor: Actor::Human,
                    rationale: None,
                    evidence: None,
                },
            )
            .await
            .unwrap();
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let detail: BuildDetail = http
            .post(format!("{base}/builds"))
            .json(&json!({
                "spec_ids": [spec.id.as_str()],
                "rationale": "why",
                "directions": "keep the migration reversible",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let directions = detail.build.directions.expect("stored on the row");
        assert_eq!(directions.text, "keep the migration reversible");
        assert_eq!(directions.author, Actor::Human);

        let decisions = store
            .decisions(Some(("build", detail.build.id.as_str())), 10)
            .await
            .unwrap();
        assert_eq!(decisions[0].rationale.as_deref(), Some("why"));
    }

    // --- build now (#869) ---

    /// The whole path in one call: the issue body becomes the spec, the spec
    /// is approved, and a build is queued over it.
    ///
    /// `ready_to_build` rather than `building` is the expected end state
    /// because no build loop runs in this test — `create_build` never touches
    /// the task, and `claim_next_queued_build` is what moves it on.
    #[tokio::test]
    async fn build_now_writes_the_issue_body_as_an_approved_spec_and_queues_a_build() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 7, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/tasks/{}/build-now", task.id))
            .json(&json!({"rationale": "the issue body is the whole spec"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let detail: BuildDetail = resp.json().await.unwrap();
        assert_eq!(detail.spec_ids.len(), 1);
        assert_eq!(detail.build.status, crate::models::BuildStatus::Queued);

        let spec = store
            .get_spec(&detail.spec_ids[0])
            .await
            .unwrap()
            .expect("the spec was written");
        assert_eq!(spec.content, task.body, "the issue body is the spec");
        assert_eq!(spec.session_id, None, "no Scout ran — that is the tell");
        assert_eq!(spec.complexity, Complexity::Simple);
        assert!(
            spec.files_touched.is_empty(),
            "nobody explored this, so nothing is known to be touched"
        );

        let entry = store.get_spec_queue_entry(&spec.id).await.unwrap().unwrap();
        assert_eq!(entry.status, SpecQueueStatus::Approved);
        assert!(entry.approved_at.is_some());
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild
        );

        // One ledger row, and it says `author_spec` rather than `approve`:
        // there was no second opinion to record.
        let decisions = store
            .decisions(Some(("spec", spec.id.as_str())), 10)
            .await
            .unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, DecisionAction::AuthorSpec);
        assert_eq!(decisions[0].actor, Actor::Human);
        assert_eq!(
            decisions[0].rationale.as_deref(),
            Some("the issue body is the whole spec")
        );
    }

    /// `content` replaces the issue body rather than extending it — the
    /// Builder prompt is spec content alone, so whatever lands here is all it
    /// reads.
    #[tokio::test]
    async fn build_now_content_replaces_the_issue_body() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 8, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/tasks/{}/build-now", task.id))
            .json(&json!({"content": "## Spec\nRename the flag.", "complexity": "medium"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let detail: BuildDetail = resp.json().await.unwrap();

        let spec = store.get_spec(&detail.spec_ids[0]).await.unwrap().unwrap();
        assert_eq!(spec.content, "## Spec\nRename the flag.");
        assert!(
            !spec.content.contains(&task.body),
            "a supplied content replaces the body, it does not append to it"
        );
        assert_eq!(spec.complexity, Complexity::Medium);
    }

    /// `content` is the specification; `directions` is what to do with it.
    /// Merging the two would put an instruction addressed to the agent inside
    /// the artifact a reviewer would read — and here that artifact is the only
    /// thing standing in for a review at all.
    #[tokio::test]
    async fn build_now_keeps_directions_out_of_the_spec_it_authors() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 9, 0).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let detail: BuildDetail = http
            .post(format!("{base}/tasks/{}/build-now", task.id))
            .json(&json!({
                "content": "## Spec\nRename the flag.",
                "rationale": "the issue says it all",
                "directions": "do not touch the migration",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let spec = store.get_spec(&detail.spec_ids[0]).await.unwrap().unwrap();
        assert_eq!(spec.content, "## Spec\nRename the flag.");
        assert!(
            !spec.content.contains("do not touch the migration"),
            "directions must not leak into the spec: {}",
            spec.content
        );
        let directions = detail.build.directions.expect("on the build row instead");
        assert_eq!(directions.text, "do not touch the migration");
        assert_eq!(directions.author, Actor::Human);
    }

    /// Human-only, and *not* charter-gated: every capability that could
    /// plausibly cover this is `live`, and it is still a 403. Authoring a
    /// spec, approving it and dispatching a build off it with no second
    /// opinion anywhere in the loop is a different autonomy from batching
    /// specs someone already ruled on.
    #[tokio::test]
    async fn build_now_refuses_the_orchestrator_however_wide_its_charter() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 9, 0).await;
        for capability in [
            Capability::DispatchBuilds,
            Capability::AutoReviewSpecs,
            Capability::QueueTasks,
        ] {
            store
                .set_charter(capability, CharterLevel::Live, None)
                .await
                .unwrap();
        }
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/tasks/{}/build-now", task.id))
            .header(
                ACTOR_HEADER,
                format!("orchestrator {}", store.actor_token().expose()),
            )
            .json(&json!({"rationale": "trivial"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        let body: Value = resp.json().await.unwrap();
        let error = body["error"].as_str().unwrap();
        assert!(
            error.contains("/scout"),
            "the refusal should point at the path that is open to it: {error}"
        );

        // Refused before anything is written — not even a shadow decision,
        // because this is not a capability that has a shadow.
        assert!(store.list_specs().await.unwrap().is_empty());
        assert!(store.list_builds().await.unwrap().is_empty());
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::Backlog
        );
    }

    /// Past `queued` a Scout has run or is running, and there is a real spec
    /// (or one on the way) to review. Writing a second one by hand there would
    /// silently supersede it.
    #[tokio::test]
    async fn build_now_refuses_a_task_the_scout_already_has() {
        let (store, project) = store_with_project().await;
        let (task, _) = insert_spec(&store, &project, 10).await;
        store
            .update_task_state(&task.id, TaskState::InReview)
            .await
            .unwrap();
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let resp = http
            .post(format!("{base}/tasks/{}/build-now", task.id))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        assert_eq!(
            store.list_specs().await.unwrap().len(),
            1,
            "the scout's spec is the only one"
        );
        assert!(store.list_builds().await.unwrap().is_empty());
    }

    /// An empty issue body and no `content` leaves nothing for the Builder to
    /// implement, and a build over an empty spec is a VM hour spent on
    /// nothing.
    #[tokio::test]
    async fn build_now_refuses_when_there_is_nothing_to_build_from() {
        let (store, project) = store_with_project().await;
        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: 11,
            title: "a title and nothing else".into(),
            body: "   ".into(),
            labels: vec![],
            gh_state: GhState::Open,
            state: TaskState::Backlog,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
            scout_directions: None,
        };
        store.insert_task(&task).await.unwrap();
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        // Sent with no body at all — the documented common case, and what a
        // bare `curl -X POST` produces. A 415 here would mean the handler was
        // never reached; the error naming the issue is what says it was.
        let resp = http
            .post(format!("{base}/tasks/{}/build-now", task.id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("#11"));
        assert!(store.list_specs().await.unwrap().is_empty());
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

    /// A router with no GitHub client answers, rather than failing: "nobody
    /// has sealed a token into this server yet" is the ordinary state of a
    /// fresh machine, and a 503 there would put a red banner on an app that is
    /// working correctly.
    #[tokio::test]
    async fn viewer_answers_unauthenticated_without_a_credential() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let base = spawn(store).await;
        let resp = reqwest::get(format!("{base}/viewer")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<Viewer>().await.unwrap(),
            Viewer::Unauthenticated
        );
    }

    /// The cache is proved by taking GitHub away between the two reads: the
    /// second one is answered from memory, so an app refreshing on every SSE
    /// event costs one GitHub call per `SUCCESS_TTL` rather than one per event.
    #[tokio::test]
    async fn viewer_is_cached_across_reads() {
        let gh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gh_url = format!("http://{}/graphql", gh_listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let gh = axum::Router::new().route(
            "/graphql",
            post(|| async {
                Json(json!({"data": {"viewer": {
                    "login": "octocat",
                    "avatarUrl": "https://avatars.example/u/9",
                    "url": "https://github.example/octocat",
                }}}))
            }),
        );
        let served = tokio::spawn(async move {
            axum::serve(gh_listener, gh)
                .with_graceful_shutdown(async {
                    stopped.await.ok();
                })
                .await
                .unwrap();
        });

        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router_with_services(
            store,
            Services {
                github: Some(Arc::new(GitHubClient::with_base_url("token", gh_url))),
                ..Default::default()
            },
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let read = async || {
            reqwest::get(format!("{base}/viewer"))
                .await
                .unwrap()
                .json::<Viewer>()
                .await
                .unwrap()
        };
        let first = read().await;
        assert_eq!(
            first,
            Viewer::Known {
                login: "octocat".into(),
                avatar_url: "https://avatars.example/u/9".into(),
                profile_url: "https://github.example/octocat".into(),
            }
        );

        // GitHub goes away entirely; the held answer must survive it.
        stop.send(()).ok();
        served.await.unwrap();
        assert_eq!(read().await, first);
    }

    /// `/status` is the answer for whoever arrives *after* the edge was
    /// announced, so it has to report a hold for as long as one lasts — and say
    /// nothing at all the rest of the time.
    ///
    /// It reads through the same `hold` predicate the two dispatchers use, so
    /// it cannot claim a hold they are not honouring; a released record must
    /// therefore clear here in the same breath.
    #[tokio::test]
    async fn status_reports_a_github_hold_for_as_long_as_it_lasts() {
        use crate::github::GhError;
        use crate::github_health::GitHubHealth;

        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let health = Arc::new(GitHubHealth::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router_with_services(
            store.clone(),
            Services {
                github_health: Some(health.clone()),
                ..Default::default()
            },
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let status = async || {
            http.get(format!("{base}/status"))
                .send()
                .await
                .unwrap()
                .json::<ServerStatus>()
                .await
                .unwrap()
        };

        assert_eq!(status().await.github, None, "quiet with nothing observed");

        let outage: Result<(), GhError> = Err(GhError::Rest {
            what: "list issues".into(),
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Service Unavailable".into(),
        });
        health.observe(&outage, Utc::now());
        health.observe(&outage, Utc::now());
        let hold = status().await.github.expect("a hold is reported");
        assert_eq!(hold.failures, 2);
        assert!(hold.error.contains("503"), "{}", hold.error);
        assert!(hold.since <= hold.last_seen);

        // Release. Without this half, "held ⇒ reported" passes just as well
        // when the predicate is stuck on.
        health.observe(&Ok(()), Utc::now());
        assert_eq!(status().await.github, None);
    }

    /// A router with no dispatchers behind it holds nothing back, so it must
    /// not claim to — an absent service is `None`, not an invented hold.
    #[tokio::test]
    async fn status_without_the_health_service_reports_no_hold() {
        let (store, _project) = store_with_project().await;
        let base = spawn(store).await;
        let status: ServerStatus = reqwest::get(format!("{base}/status"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status.github, None);
    }

    /// `/status` answers both halves of "is it up?" in one call: the process
    /// facts only this pid can supply, and the store facts a supervisor needs
    /// before it signals anything.
    #[tokio::test]
    async fn status_answers_process_and_store_facts_together() {
        let (store, project) = store_with_project().await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let body: Value = http
            .get(format!("{base}/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["pid"], std::process::id());
        assert_eq!(body["mode"], "pause");
        assert!(body["started_at"].is_string());
        // An in-memory store is a first boot, so it migrated everything.
        assert!(!body["migrations_applied"].as_array().unwrap().is_empty());
        assert_eq!(body["in_flight"]["scouts"].as_array().unwrap().len(), 0);
        assert!(body["in_flight"]["orchestrator"].is_null());

        // A running scout shows up with the task behind it.
        let task = insert_task(&store, &project, 1, 0).await;
        let session = Session {
            id: SessionId::new(),
            task_id: task.id.clone(),
            vm_id: Some("vm-1".into()),
            branch: "scout/1".into(),
            status: SessionStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            exit_reason: None,
            usage: None,
            directions: None,
        };
        store.insert_session(&session).await.unwrap();

        let body: Value = http
            .get(format!("{base}/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let scouts = body["in_flight"]["scouts"].as_array().unwrap();
        assert_eq!(scouts.len(), 1);
        assert_eq!(scouts[0]["id"], session.id.to_string());
        assert!(scouts[0]["since"].is_string());

        // And the typed shape the client and the supervisor decode.
        let typed: ServerStatus = http
            .get(format!("{base}/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(typed.pid, std::process::id());
        assert!(typed.in_flight.is_destructible());
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
                status: ProjectStatus::Active,
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
            directions: None,
        };
        store.insert_session(&session).await.unwrap();
        session.id
    }

    // --- scout notes (#835) ---

    /// Salvage is reachable by session, and only by session: a 404 for a run
    /// that left nothing, and no route from `/specs` or `/spec-queue`.
    #[tokio::test]
    async fn session_notes_are_served_alone_and_404_when_there_are_none() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let session_id = seed_session(&store).await;
        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let missing = http
            .get(format!("{base}/sessions/{session_id}/notes"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        let session = store.get_session(&session_id).await.unwrap().unwrap();
        store
            .upsert_scout_notes(&ScoutNotes {
                session_id: session_id.clone(),
                task_id: session.task_id.clone(),
                reason: Some("scout timed out after 3600s".into()),
                notes: "# Notes\n\nGot as far as the parser.".into(),
                files_touched: vec!["src/parse.rs".into()],
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let notes: ScoutNotes = http
            .get(format!("{base}/sessions/{session_id}/notes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(notes.session_id, session_id);
        assert!(notes.notes.contains("Got as far as the parser."));
        assert_eq!(notes.reason.as_deref(), Some("scout timed out after 3600s"));

        // The invariant, from the API's side: salvage has no review path.
        let specs: Vec<Spec> = http
            .get(format!("{base}/specs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(specs.is_empty());
        let queue: Vec<SpecQueueItem> = http
            .get(format!("{base}/spec-queue"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn transcript_reads_page_and_honour_inclusive_since() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let session_id = seed_session(&store).await;
        let batch: Vec<_> = (1..=5)
            .map(|i| (crate::models::TranscriptStream::Stdout, format!("line {i}")))
            .collect();
        store
            .append_transcript_lines(&TranscriptOwner::session(&session_id), &batch)
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

    /// Builds read on the same contract as sessions, through their own route,
    /// and a line says which owner it belongs to (#825).
    #[tokio::test]
    async fn a_build_transcript_reads_on_the_same_contract_as_a_session() {
        let (store, project) = store_with_project().await;
        let (_task, spec) = insert_spec(&store, &project, 1).await;
        store
            .review_spec(
                &spec.id,
                SpecQueueStatus::Approved,
                None,
                DecisionInput::human(),
            )
            .await
            .unwrap();
        let build = store
            .create_build(
                std::slice::from_ref(&spec.id),
                "main",
                DecisionInput::human(),
            )
            .await
            .unwrap();
        let owner = TranscriptOwner::build(&build.id);
        let batch: Vec<_> = (1..=3)
            .map(|i| (crate::models::TranscriptStream::Stdout, format!("line {i}")))
            .collect();
        store.append_transcript_lines(&owner, &batch).await.unwrap();

        let base = spawn(store.clone()).await;
        let http = reqwest::Client::new();

        let all: Vec<TranscriptLine> = http
            .get(format!("{base}/builds/{}/transcript", build.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|l| l.owner == owner));
        assert_eq!(all[0].seq, 1, "a build's transcript starts at seq 1");

        // `since` is inclusive here too.
        let tail: Vec<TranscriptLine> = http
            .get(format!("{base}/builds/{}/transcript?since=3", build.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(tail.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![3]);

        // And the owner is a resource, not a free-form id.
        let missing = http
            .get(format!("{base}/builds/build_nope/transcript"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
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
            .append_transcript_lines(&TranscriptOwner::session(&session_id), &batch)
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
                &TranscriptOwner::session(&session_id),
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
                &TranscriptOwner::session(&session_id),
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
            scout_directions: None,
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

    // --- the loopback guard (#985) -------------------------------------

    /// A path with no route answers **403 rather than 404**.
    ///
    /// This is what pins the layer as outermost, and therefore that the
    /// property holds for routes nobody has written yet. Unwrap the layer —
    /// or chain it inside `routes` instead of around it — and this is the
    /// test that goes red.
    #[tokio::test]
    async fn the_guard_covers_a_path_with_no_route() {
        let (store, _project) = store_with_project().await;
        let base = spawn(store).await;
        let http = reqwest::Client::new();

        // Control: the same unrouted path, no offending header, is a 404 —
        // so the 403 below is the guard and not a route that never existed.
        let resp = http
            .get(format!("{base}/no-such-route"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let resp = http
            .get(format!("{base}/no-such-route"))
            .header("origin", "https://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            403,
            "an unrouted path must be refused by the guard before it is a 404"
        );
    }

    /// The issue's path 1: a CORS-*simple* `POST` — no body, no
    /// `Content-Type`, therefore no preflight — from any page you have open.
    /// The opaque response does not matter, because the VM is already
    /// dispatched.
    #[tokio::test]
    async fn a_cross_origin_post_cannot_reach_a_handler() {
        let (store, project) = store_with_project().await;
        let task = insert_task(&store, &project, 1, 0).await;
        let base = spawn(store).await;
        let http = reqwest::Client::new();

        for path in [
            format!("/tasks/{}/build-now", task.id),
            format!("/tasks/{}/scout", task.id),
            format!("/tasks/{}/queue", task.id),
            "/runs/cancel-all".to_string(),
        ] {
            // Control: without the header the same call reaches the handler,
            // so the assertion below is about the header and not about a
            // route that was broken anyway.
            let resp = http.post(format!("{base}{path}")).send().await.unwrap();
            assert_ne!(resp.status(), 403, "control: {path} without an Origin");

            let resp = http
                .post(format!("{base}{path}"))
                .header("origin", "https://evil.example")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403, "cross-origin POST to {path}");
        }
    }

    /// The issue's path 2: DNS rebinding. A name the attacker controls
    /// resolving to `127.0.0.1` makes their page genuinely same-origin, which
    /// lifts the simple-request restriction entirely — so reads matter as
    /// much as writes here.
    #[tokio::test]
    async fn a_rebound_host_can_neither_read_nor_write() {
        let (store, _project) = store_with_project().await;
        let base = spawn(store).await;
        let http = reqwest::Client::new();

        for (method, path) in [
            ("GET", "/tasks"),
            ("GET", "/decisions"),
            ("GET", "/status"),
            ("GET", "/version"),
            ("POST", "/pull-requests/1/merge"),
        ] {
            let url = format!("{base}{path}");
            let request = |host: Option<&str>| {
                let builder = match method {
                    "GET" => http.get(&url),
                    _ => http.post(&url),
                };
                match host {
                    Some(host) => builder.header("host", host),
                    None => builder,
                }
            };

            // Control: the same call with the real loopback authority.
            let resp = request(None).send().await.unwrap();
            assert_ne!(resp.status(), 403, "control: {method} {path}");

            let resp = request(Some("evil.example:4800")).send().await.unwrap();
            assert_eq!(resp.status(), 403, "rebound {method} {path}");
        }
    }

    /// The three authorities every real client in this tree sends: `ureq`
    /// (the app, `tasks status`, `tasks-client`) and `reqwest` (`tasks
    /// reload`) both derive `Host` from the URL, and the orchestrator's
    /// `curl -K` config carries `X-Tasks-Actor` and nothing else.
    #[tokio::test]
    async fn the_shapes_real_clients_send_are_untouched() {
        let (store, _project) = store_with_project().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, router(store)).await.unwrap();
        });
        let http = reqwest::Client::new();

        for host in [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ] {
            let resp = http
                .get(format!("http://127.0.0.1:{port}/version"))
                .header("host", &host)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{host} should be accepted");
        }
    }
}
