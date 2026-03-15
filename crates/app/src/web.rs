//! Web API — spec Section 3.1, 16.3.
//!
//! HTTP REST API and SSE event stream for the web GUI.
//! The server serves the built frontend as static files and
//! exposes API endpoints under `/api/`.

use std::sync::Arc;
use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;

use events::Actor;
use server::mode::Mode;
use server::Server;

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub server: Arc<Server>,
    pub max_sessions: u32,
}

/// Build the API router.
pub fn router(state: ApiState) -> Router {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/events", get(get_task_events))
        .route("/projects", get(list_projects))
        .route("/merge-queue", get(list_merge_queue))
        .route("/merge-queue/flush", post(flush_merge_queue))
        .route("/merge-queue/{id}/approve", post(approve_merge))
        .route("/merge-queue/{id}/reject", post(reject_merge))
        .route("/mode", get(get_mode))
        .route("/mode", post(set_mode))
        .route("/events", get(event_stream));

    Router::new()
        .nest("/api", api)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// --- Response types ---

#[derive(Serialize)]
struct SnapshotResponse {
    mode: Mode,
    projects: Vec<models::project::Project>,
    tasks: Vec<models::task::Task>,
    merge_queue: Vec<models::merge_queue::MergeQueueEntry>,
    slot_utilization: SlotUtilization,
    human_present: bool,
}

#[derive(Serialize)]
struct SlotUtilization {
    active: u32,
    max: u32,
}

#[derive(Serialize)]
struct ModeResponse {
    mode: Mode,
}

#[derive(Deserialize)]
struct SetModeRequest {
    mode: Mode,
}

#[derive(Deserialize)]
struct EventStreamQuery {
    /// Optional event type pattern filter (e.g. "task:*", "agent:message").
    pattern: Option<String>,
    /// Optional task ID filter.
    task_id: Option<String>,
}

// --- Handlers ---

/// GET /api/snapshot — Full system state (spec §16.3).
async fn snapshot(State(state): State<ApiState>) -> Json<SnapshotResponse> {
    let server_state = state.server.state.read().await;
    let active = server_state
        .tasks
        .values()
        .filter(|t| {
            matches!(
                t.state,
                models::task::TaskState::Running
                    | models::task::TaskState::Question
                    | models::task::TaskState::Testing
            )
        })
        .count() as u32;

    Json(SnapshotResponse {
        mode: server_state.mode,
        projects: server_state.projects.values().cloned().collect(),
        tasks: server_state.tasks.values().cloned().collect(),
        merge_queue: server_state.merge_queue.entries().to_vec(),
        slot_utilization: SlotUtilization {
            active,
            max: state.max_sessions,
        },
        human_present: state.server.is_human_present(),
    })
}

/// GET /api/tasks — List all tasks.
async fn list_tasks(State(state): State<ApiState>) -> Json<Vec<models::task::Task>> {
    let server_state = state.server.state.read().await;
    let mut tasks: Vec<_> = server_state.tasks.values().cloned().collect();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(tasks)
}

/// GET /api/tasks/:id — Get a single task.
async fn get_task(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<models::task::Task>, StatusCode> {
    let server_state = state.server.state.read().await;
    server_state
        .tasks
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// GET /api/tasks/:id/events — Get event history for a task.
async fn get_task_events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<events::Event>>, StatusCode> {
    state
        .server
        .event_bus
        .read_task(&id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// GET /api/projects — List all projects.
async fn list_projects(
    State(state): State<ApiState>,
) -> Json<Vec<models::project::Project>> {
    let server_state = state.server.state.read().await;
    Json(server_state.projects.values().cloned().collect())
}

/// GET /api/merge-queue — List merge queue entries.
async fn list_merge_queue(
    State(state): State<ApiState>,
) -> Json<Vec<models::merge_queue::MergeQueueEntry>> {
    let server_state = state.server.state.read().await;
    Json(server_state.merge_queue.entries().to_vec())
}

/// POST /api/merge-queue/flush — Flush approved entries (Pause mode only).
async fn flush_merge_queue(State(state): State<ApiState>) -> Result<Json<Vec<String>>, ApiError> {
    state
        .server
        .flush_merge_queue()
        .await
        .map(Json)
        .map_err(ApiError::Server)
}

/// POST /api/merge-queue/:id/approve — Approve a merge queue entry.
async fn approve_merge(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut server_state = state.server.state.write().await;
    server_state
        .merge_queue
        .approve(&id)
        .map_err(|e| ApiError::MergeQueue(e.to_string()))?;
    Ok(StatusCode::OK)
}

/// POST /api/merge-queue/:id/reject — Reject a merge queue entry.
async fn reject_merge(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut server_state = state.server.state.write().await;
    server_state
        .merge_queue
        .reject(&id)
        .map_err(|e| ApiError::MergeQueue(e.to_string()))?;
    Ok(StatusCode::OK)
}

/// GET /api/mode — Get current operating mode.
async fn get_mode(State(state): State<ApiState>) -> Json<ModeResponse> {
    Json(ModeResponse {
        mode: state.server.mode().await,
    })
}

/// POST /api/mode — Set operating mode.
async fn set_mode(
    State(state): State<ApiState>,
    Json(req): Json<SetModeRequest>,
) -> Result<Json<ModeResponse>, ApiError> {
    let mode = state
        .server
        .set_mode(req.mode, &Actor::Human)
        .await
        .map_err(ApiError::Server)?;
    Ok(Json(ModeResponse { mode }))
}

/// GET /api/events — SSE stream of live events.
///
/// Supports optional query params: `pattern` and `task_id` for filtering.
async fn event_stream(
    State(state): State<ApiState>,
    Query(query): Query<EventStreamQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // Register presence — the connection guard will decrement on drop.
    let _presence_guard = state.server.presence.connect();

    let rx = state.server.event_bus.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                // Apply filters
                if let Some(ref pattern) = query.pattern {
                    if !event.event_type.matches(pattern) {
                        return None;
                    }
                }
                if let Some(ref task_id) = query.task_id {
                    if event.task != *task_id {
                        return None;
                    }
                }
                let data = serde_json::to_string(event.as_ref()).ok()?;
                Some(Ok(SseEvent::default().data(data)))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// --- Error handling ---

enum ApiError {
    Server(server::ServerError),
    MergeQueue(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Server(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            ApiError::MergeQueue(e) => (StatusCode::BAD_REQUEST, e),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
