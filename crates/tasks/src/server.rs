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
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tracing::{error, info, warn};

use crate::events::{Event, EventPayload};
use crate::models::{
    Mode, Project, ProjectId, Session, SessionId, Spec, SpecId, SpecQueueItem, SpecQueueStatus,
    Task, TaskId,
};
use crate::store::{Store, StoreError};

/// How many events `/events` returns when the caller doesn't ask for a count.
const DEFAULT_EVENT_LIMIT: i64 = 100;

/// Interval between SSE keep-alive comments.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
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
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Build the API router over a store. Exposed separately from [`serve`] so
/// tests can bind their own listener.
pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/projects", get(list_projects).post(create_project))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{task_id}", get(get_task))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{session_id}", get(get_session))
        .route("/specs", get(list_specs))
        .route("/specs/{spec_id}", get(get_spec))
        .route("/spec-queue", get(list_spec_queue))
        .route("/spec-queue/reorder", post(reorder_spec_queue))
        .route("/spec-queue/{spec_id}/review", post(review_spec))
        .route("/mode", get(get_mode).post(set_mode))
        .route("/queue/reorder", post(reorder_queue))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .with_state(store)
}

/// Serve the API on loopback at `port`. Runs until the process is killed.
pub async fn serve(store: Arc<Store>, port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    info!(addr = %listener.local_addr()?, "tasks api listening");
    axum::serve(listener, router(store)).await
}

// --- projects ---

async fn list_projects(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(store.list_projects().await?))
}

#[derive(Debug, Deserialize)]
struct CreateProject {
    repo_owner: String,
    repo_name: String,
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

async fn list_tasks(State(store): State<Arc<Store>>) -> ApiResult<Json<Vec<Task>>> {
    Ok(Json(store.list_tasks().await?))
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

#[derive(Debug, Deserialize)]
struct ReorderQueue {
    task_ids: Vec<TaskId>,
}

async fn reorder_queue(
    State(store): State<Arc<Store>>,
    Json(body): Json<ReorderQueue>,
) -> ApiResult<Json<Vec<Task>>> {
    store.set_queue_order(&body.task_ids).await?;
    Ok(Json(store.list_tasks().await?))
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

#[derive(Debug, Deserialize)]
struct ReorderSpecQueue {
    spec_ids: Vec<SpecId>,
}

async fn reorder_spec_queue(
    State(store): State<Arc<Store>>,
    Json(body): Json<ReorderSpecQueue>,
) -> ApiResult<Json<Vec<SpecQueueItem>>> {
    store.set_spec_queue_order(&body.spec_ids).await?;
    Ok(Json(store.list_spec_queue().await?))
}

/// Review verdict. `status` is taken as a string rather than a
/// [`SpecQueueStatus`] so an unknown value is a 400 from us instead of a
/// deserialization rejection.
#[derive(Debug, Deserialize)]
struct ReviewRequest {
    status: String,
    #[serde(default)]
    feedback: Option<String>,
}

async fn review_spec(
    State(store): State<Arc<Store>>,
    Path(spec_id): Path<String>,
    Json(body): Json<ReviewRequest>,
) -> ApiResult<Json<SpecQueueItem>> {
    let status = SpecQueueStatus::from_str(&body.status)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown status: {}", body.status)))?;
    let id = SpecId::from_raw(spec_id);
    let entry = store.review_spec(&id, status, body.feedback).await?;
    let spec = store
        .get_spec(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("spec {id}")))?;
    Ok(Json(SpecQueueItem {
        entry,
        task_id: spec.task_id,
    }))
}

// --- mode ---

#[derive(Debug, Serialize)]
struct ModeResponse {
    mode: Mode,
}

#[derive(Debug, Deserialize)]
struct SetMode {
    mode: String,
}

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
        Some(since) => {
            let mut events = store.events_since(since).await?;
            events.truncate(limit as usize);
            events
        }
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
    use serde_json::Value;

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
            state: TaskState::New,
            priority,
            manual_rank: None,
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

        let events = store.events_since(0).await.unwrap();
        assert_eq!(
            events[0].payload,
            EventPayload::ProjectAdded {
                project_id: created.id
            }
        );
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
            TaskState::Queued
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
            TaskState::New
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

        let events = store.events_since(0).await.unwrap();
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
}
