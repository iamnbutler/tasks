//! Web API — spec Section 3.1, 16.3.
//!
//! HTTP REST API and SSE event stream for the web GUI.
//! The server serves the built frontend as static files and
//! exposes API endpoints under `/api/`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;

use events::Actor;
use server::Server;
use server::mode::Mode;
use tasks_agent::CompletionsService;

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub server: Arc<Server>,
    pub max_sessions: u32,
    pub session_manager: Option<Arc<tasks_session::SessionManager<runtime::AppleContainerRuntime>>>,
    pub completions_service: Option<Arc<RwLock<CompletionsService>>>,
}

/// Build the API router.
pub fn router(state: ApiState) -> Router {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/events", get(get_task_events))
        .route("/tasks/{id}/chat", post(send_chat))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/projects", get(list_projects))
        .route("/projects", post(add_project))
        .route("/projects/{id}", axum::routing::delete(delete_project))
        .route("/merge-queue", get(list_merge_queue))
        .route("/merge-queue/flush", post(flush_merge_queue))
        .route("/merge-queue/{id}/approve", post(approve_merge))
        .route("/merge-queue/{id}/reject", post(reject_merge))
        .route("/mode", get(get_mode))
        .route("/mode", post(set_mode))
        .route("/events", get(event_stream))
        // Completions endpoints (fast mode LLM service)
        .route("/completions", post(completions))
        .route("/completions/name", post(completions_name))
        .route("/completions/describe", post(completions_describe))
        .route("/completions/brainstorm", post(completions_brainstorm))
        .route("/completions/summarize", post(completions_summarize));

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
struct AddProjectRequest {
    repo: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Deserialize)]
struct EventStreamQuery {
    /// Optional event type pattern filter (e.g. "task:*", "agent:message").
    pattern: Option<String>,
    /// Optional task ID filter.
    task_id: Option<String>,
}

// --- Completions request/response types ---

#[derive(Deserialize)]
struct CompletionsRequest {
    /// The prompt to complete.
    prompt: String,
    /// Optional system prompt.
    system: Option<String>,
    /// Optional temperature (0.0-1.0).
    temperature: Option<f32>,
    /// Optional max tokens override.
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct CompletionsResponse {
    /// The generated text.
    text: String,
    /// Token usage information.
    usage: Option<CompletionsUsage>,
}

#[derive(Serialize)]
struct CompletionsUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize)]
struct ContextRequest {
    /// The context to use for generation.
    context: String,
}

#[derive(Deserialize)]
struct BrainstormRequest {
    /// The context to brainstorm about.
    context: String,
    /// Number of suggestions to generate (default 5).
    count: Option<u32>,
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
async fn list_projects(State(state): State<ApiState>) -> Json<Vec<models::project::Project>> {
    let server_state = state.server.state.read().await;
    Json(server_state.projects.values().cloned().collect())
}

/// POST /api/projects — Add a new project.
async fn add_project(
    State(state): State<ApiState>,
    Json(req): Json<AddProjectRequest>,
) -> Result<Json<models::project::Project>, ApiError> {
    let parts: Vec<&str> = req.repo.split('/').collect();
    if parts.len() != 2 {
        return Err(ApiError::BadRequest(format!(
            "Invalid repo format: {} (expected owner/repo)",
            req.repo
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let project = models::project::Project::new(&id, &req.repo);
    state.server.add_project(project.clone()).await;
    Ok(Json(project))
}

/// DELETE /api/projects/:id — Remove a project.
async fn delete_project(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = state.server.remove_project(&id).await;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::BadRequest(format!("Project not found: {id}")))
    }
}

/// GET /api/merge-queue — List merge queue entries.
async fn list_merge_queue(
    State(state): State<ApiState>,
) -> Json<Vec<models::merge_queue::MergeQueueEntry>> {
    let server_state = state.server.state.read().await;
    Json(server_state.merge_queue.entries().to_vec())
}

/// POST /api/merge-queue/flush — Flush approved entries (Pause mode only).
///
/// Flushes approved entries and executes the actual GitHub merges.
async fn flush_merge_queue(State(state): State<ApiState>) -> Result<Json<Vec<String>>, ApiError> {
    let flushed_entries = state
        .server
        .flush_merge_queue()
        .await
        .map_err(ApiError::Server)?;

    // Execute GitHub merges for each flushed entry
    let github_token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if !github_token.is_empty() {
        let client = tasks_github::client::GitHubClient::new(&github_token);
        for (entry_id, pr_url) in &flushed_entries {
            if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(pr_url) {
                match client.merge_pull_request(&owner, &repo, number).await {
                    Ok(true) => {
                        tracing::info!(entry_id = %entry_id, pr_url = %pr_url, "PR merged via flush");
                        if let Err(e) = state.server.mark_entry_merged(entry_id, pr_url).await {
                            tracing::error!(entry_id = %entry_id, error = %e, "failed to mark entry merged after flush");
                        }
                    }
                    Ok(false) => {
                        tracing::warn!(entry_id = %entry_id, pr_url = %pr_url, "PR not mergeable during flush");
                        if let Err(e) = state.server.mark_entry_conflict(entry_id, pr_url).await {
                            tracing::error!(entry_id = %entry_id, error = %e, "failed to mark entry conflict after flush");
                        }
                    }
                    Err(e) => {
                        tracing::error!(entry_id = %entry_id, pr_url = %pr_url, error = %e, "failed to merge PR during flush");
                    }
                }
            }
        }
    }

    let ids: Vec<String> = flushed_entries.into_iter().map(|(id, _)| id).collect();
    Ok(Json(ids))
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

/// POST /api/tasks/:id/chat — Send a chat message to a running session (spec §9.2).
async fn send_chat(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ChatRequest>,
) -> Result<StatusCode, ApiError> {
    let sm = state
        .session_manager
        .as_ref()
        .ok_or_else(|| ApiError::SessionManager("session manager not available".into()))?;

    // Emit HumanMessage event before sending to session.
    // This event triggers dispatch if the task is in Question state (spec §12.1).
    let event = events::Event::new(
        events::EventType::HumanMessage,
        &id,
        Actor::Human,
        serde_json::json!({
            "message": req.message,
        }),
    );
    state
        .server
        .event_bus
        .publish(event)
        .await
        .map_err(|e| ApiError::Server(server::ServerError::EventStore(e)))?;

    sm.send_chat(&id, req.message)
        .await
        .map_err(|e| ApiError::SessionManager(e.to_string()))?;
    Ok(StatusCode::OK)
}

/// POST /api/tasks/:id/cancel — Stop a running session (spec §9.5).
async fn cancel_task(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sm = state
        .session_manager
        .as_ref()
        .ok_or_else(|| ApiError::SessionManager("session manager not available".into()))?;
    sm.stop_session(&id)
        .await
        .map_err(|e| ApiError::SessionManager(e.to_string()))?;
    Ok(StatusCode::OK)
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

// --- Completions handlers ---

/// POST /api/completions — General-purpose text completion.
///
/// Uses claude-haiku-4-5 for fast responses. Supports optional system prompt,
/// temperature, and max_tokens parameters.
async fn completions(
    State(state): State<ApiState>,
    Json(req): Json<CompletionsRequest>,
) -> Result<Json<CompletionsResponse>, ApiError> {
    let service_arc = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::Completions("completions service not available".into()))?;

    // Clone the service to avoid holding lock across await
    let service: CompletionsService = service_arc.read().await.clone();
    let response = service
        .complete_advanced(
            req.system.as_deref(),
            vec![tasks_agent::Message::user(&req.prompt)],
            req.temperature,
            req.max_tokens,
        )
        .await
        .map_err(|e: tasks_agent::AgentError| ApiError::Completions(e.to_string()))?;

    Ok(Json(CompletionsResponse {
        text: response.text(),
        usage: response.usage.map(|u| CompletionsUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        }),
    }))
}

/// POST /api/completions/name — Generate a name.
///
/// Utility endpoint that generates a single, concise name based on context.
async fn completions_name(
    State(state): State<ApiState>,
    Json(req): Json<ContextRequest>,
) -> Result<Json<CompletionsResponse>, ApiError> {
    let service_arc = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::Completions("completions service not available".into()))?;

    // Clone the service to avoid holding lock across await
    let service: CompletionsService = service_arc.read().await.clone();
    let text = service
        .generate_name(&req.context)
        .await
        .map_err(|e: tasks_agent::AgentError| ApiError::Completions(e.to_string()))?;

    Ok(Json(CompletionsResponse { text, usage: None }))
}

/// POST /api/completions/describe — Generate a description.
///
/// Utility endpoint that generates a clear, concise description (1-2 sentences).
async fn completions_describe(
    State(state): State<ApiState>,
    Json(req): Json<ContextRequest>,
) -> Result<Json<CompletionsResponse>, ApiError> {
    let service_arc = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::Completions("completions service not available".into()))?;

    // Clone the service to avoid holding lock across await
    let service: CompletionsService = service_arc.read().await.clone();
    let text = service
        .generate_description(&req.context)
        .await
        .map_err(|e: tasks_agent::AgentError| ApiError::Completions(e.to_string()))?;

    Ok(Json(CompletionsResponse { text, usage: None }))
}

/// POST /api/completions/brainstorm — Brainstorm names or ideas.
///
/// Utility endpoint that generates multiple suggestions based on context.
async fn completions_brainstorm(
    State(state): State<ApiState>,
    Json(req): Json<BrainstormRequest>,
) -> Result<Json<CompletionsResponse>, ApiError> {
    let service_arc = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::Completions("completions service not available".into()))?;

    // Clone the service to avoid holding lock across await
    let service: CompletionsService = service_arc.read().await.clone();
    let text = service
        .brainstorm(&req.context, req.count)
        .await
        .map_err(|e: tasks_agent::AgentError| ApiError::Completions(e.to_string()))?;

    Ok(Json(CompletionsResponse { text, usage: None }))
}

/// POST /api/completions/summarize — Summarize text content.
///
/// Utility endpoint that provides a clear, concise summary.
async fn completions_summarize(
    State(state): State<ApiState>,
    Json(req): Json<ContextRequest>,
) -> Result<Json<CompletionsResponse>, ApiError> {
    let service_arc = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::Completions("completions service not available".into()))?;

    // Clone the service to avoid holding lock across await
    let service: CompletionsService = service_arc.read().await.clone();
    let text = service
        .summarize(&req.context)
        .await
        .map_err(|e: tasks_agent::AgentError| ApiError::Completions(e.to_string()))?;

    Ok(Json(CompletionsResponse { text, usage: None }))
}

// --- Error handling ---

enum ApiError {
    Server(server::ServerError),
    BadRequest(String),
    MergeQueue(String),
    SessionManager(String),
    Completions(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Server(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            ApiError::BadRequest(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::MergeQueue(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::SessionManager(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::Completions(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
