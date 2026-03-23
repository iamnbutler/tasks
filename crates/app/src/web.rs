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
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;

use tasks_agent::CompletionsService;
use events::Actor;
use server::Server;
use models::Mode;

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub server: Arc<Server>,
    pub max_sessions: u32,
    pub session_manager: Option<Arc<tasks_session::SessionManager<runtime::AppleContainerRuntime>>>,
    pub completions_service: Option<Arc<CompletionsService>>,
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
        .route("/rebuild", post(rebuild_from_github))
        .route("/orchestrator/chat", post(orchestrator_chat))
        .route("/accounting", get(get_accounting_summary))
        .route("/accounting/tasks", get(list_task_accounting))
        .route("/accounting/tasks/{id}", get(get_task_accounting))
        .route("/events", get(event_stream))
        // Completions endpoints (fast mode with Haiku)
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
struct OrchestratorChatRequest {
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
struct CompletionRequest {
    /// The prompt to send to the model.
    prompt: String,
    /// Optional system prompt.
    #[serde(default)]
    system: Option<String>,
    /// Maximum tokens to generate (default: 1024).
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct CompletionResponse {
    /// The generated text.
    text: String,
}

#[derive(Deserialize)]
struct NameRequest {
    /// Context to generate a name from (e.g., task title + summary).
    context: String,
}

#[derive(Serialize)]
struct NameResponse {
    /// The generated name.
    name: String,
}

#[derive(Deserialize)]
struct DescribeRequest {
    /// Context to describe.
    context: String,
}

#[derive(Serialize)]
struct DescribeResponse {
    /// The generated description.
    description: String,
}

#[derive(Deserialize)]
struct BrainstormRequest {
    /// Topic to brainstorm about.
    topic: String,
    /// Number of ideas to generate (default: 5).
    #[serde(default)]
    count: Option<u32>,
}

#[derive(Serialize)]
struct BrainstormResponse {
    /// The generated ideas.
    ideas: Vec<String>,
}

#[derive(Deserialize)]
struct SummarizeRequest {
    /// Text to summarize.
    text: String,
    /// Optional maximum word count for summary.
    #[serde(default)]
    max_words: Option<u32>,
}

#[derive(Serialize)]
struct SummarizeResponse {
    /// The summary.
    summary: String,
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
                        if let Err(e) = state.server.mark_entry_conflict(entry_id, pr_url, None).await {
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
///
/// When transitioning to Stop mode, all running sessions are terminated.
/// Sessions get 5 seconds to stop gracefully before containers are force-destroyed.
/// (Spec §6.1: "Running agent processes are terminated")
async fn set_mode(
    State(state): State<ApiState>,
    Json(req): Json<SetModeRequest>,
) -> Result<Json<ModeResponse>, ApiError> {
    let mode = state
        .server
        .set_mode(req.mode, &Actor::Human)
        .await
        .map_err(ApiError::Server)?;

    // When entering Stop mode, terminate all running sessions (spec §6.1).
    // TODO: Move to an event-driven listener (on system:mode:stop) so that
    // non-web mode changes (CLI, orchestrator) also trigger session termination.
    if mode == Mode::Stop {
        if let Some(ref session_manager) = state.session_manager {
            // Give sessions 5 seconds to stop gracefully before force-destroying containers
            let timeout = std::time::Duration::from_secs(5);
            let stopped = session_manager.stop_all_with_timeout(timeout).await;
            if stopped > 0 {
                tracing::info!(stopped_sessions = stopped, "terminated sessions for Stop mode");
            }
        }
    }

    Ok(Json(ModeResponse { mode }))
}

/// POST /api/rebuild — Rebuild state from GitHub (issue #256).
///
/// Clears tasks and merge queue from both memory and database,
/// then signals the poll loop to re-fetch all data from GitHub.
/// Preserves: accounting data, event logs, projects table, operating mode.
async fn rebuild_from_github(
    State(state): State<ApiState>,
) -> Result<Json<server::RebuildStats>, ApiError> {
    let stats = state
        .server
        .rebuild_from_github()
        .await
        .map_err(ApiError::Server)?;

    Ok(Json(stats))
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

/// POST /api/orchestrator/chat — Send a message to the orchestrator.
///
/// This emits an orchestrator:message event that can be picked up by the orchestrator
/// to process user requests or questions.
async fn orchestrator_chat(
    State(state): State<ApiState>,
    Json(req): Json<OrchestratorChatRequest>,
) -> Result<StatusCode, ApiError> {
    // Emit OrchestratorMessage event with the human's message.
    // The orchestrator can pick this up and respond accordingly.
    let event = events::Event::new(
        events::EventType::OrchestratorMessage,
        "", // No specific task - this is a system-wide message
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

    Ok(StatusCode::OK)
}

// --- Accounting endpoints (spec §16.4) ---

/// GET /api/accounting — Global accounting summary.
async fn get_accounting_summary(
    State(state): State<ApiState>,
) -> Result<Json<tasks_store::AccountingSummary>, ApiError> {
    let summary = state.server.get_accounting_summary()
        .map_err(ApiError::Server)?;
    Ok(Json(summary))
}

/// GET /api/accounting/tasks — List all task accounting summaries.
async fn list_task_accounting(
    State(state): State<ApiState>,
) -> Result<Json<Vec<tasks_store::TaskAccounting>>, ApiError> {
    let accounting = state.server.list_task_accounting()
        .map_err(ApiError::Server)?;
    Ok(Json(accounting))
}

/// GET /api/accounting/tasks/:id — Get accounting for a specific task.
async fn get_task_accounting(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<tasks_store::TaskAccounting>, ApiError> {
    let accounting = state.server.get_task_accounting(&id)
        .map_err(ApiError::Server)?
        .ok_or_else(|| ApiError::NotFound(format!("accounting not found for task: {id}")))?;
    Ok(Json(accounting))
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

/// Maximum input size for completions endpoints (32 KB).
const MAX_COMPLETION_INPUT_BYTES: usize = 32_768;

fn validate_input_size(input: &str) -> Result<(), ApiError> {
    if input.len() > MAX_COMPLETION_INPUT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "input exceeds maximum size of {} bytes",
            MAX_COMPLETION_INPUT_BYTES
        )));
    }
    Ok(())
}

/// POST /api/completions — General completion endpoint.
///
/// Send a prompt and optionally a system prompt for fast LLM completions using Haiku.
async fn completions(
    State(state): State<ApiState>,
    Json(req): Json<CompletionRequest>,
) -> Result<Json<CompletionResponse>, ApiError> {
    validate_input_size(&req.prompt)?;
    if let Some(ref system) = req.system {
        validate_input_size(system)?;
    }

    let service = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::CompletionsUnavailable("completions service not configured".into()))?;

    let text = if let Some(system) = req.system {
        service
            .complete_with_system(&system, &req.prompt, req.max_tokens)
            .await
            .map_err(|e| ApiError::CompletionsError(e.to_string()))?
    } else {
        service
            .complete(&req.prompt, req.max_tokens)
            .await
            .map_err(|e| ApiError::CompletionsError(e.to_string()))?
    };

    Ok(Json(CompletionResponse { text }))
}

/// POST /api/completions/name — Generate a name.
///
/// Given context (e.g., task title + summary), generates a concise name.
async fn completions_name(
    State(state): State<ApiState>,
    Json(req): Json<NameRequest>,
) -> Result<Json<NameResponse>, ApiError> {
    validate_input_size(&req.context)?;

    let service = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::CompletionsUnavailable("completions service not configured".into()))?;

    let name = service
        .generate_name(&req.context)
        .await
        .map_err(|e| ApiError::CompletionsError(e.to_string()))?;

    Ok(Json(NameResponse { name }))
}

/// POST /api/completions/describe — Generate a description.
///
/// Given context, generates a brief description (1-2 sentences).
async fn completions_describe(
    State(state): State<ApiState>,
    Json(req): Json<DescribeRequest>,
) -> Result<Json<DescribeResponse>, ApiError> {
    validate_input_size(&req.context)?;

    let service = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::CompletionsUnavailable("completions service not configured".into()))?;

    let description = service
        .generate_description(&req.context)
        .await
        .map_err(|e| ApiError::CompletionsError(e.to_string()))?;

    Ok(Json(DescribeResponse { description }))
}

/// POST /api/completions/brainstorm — Brainstorm ideas.
///
/// Given a topic, generates a list of creative ideas.
async fn completions_brainstorm(
    State(state): State<ApiState>,
    Json(req): Json<BrainstormRequest>,
) -> Result<Json<BrainstormResponse>, ApiError> {
    validate_input_size(&req.topic)?;

    let service = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::CompletionsUnavailable("completions service not configured".into()))?;

    let ideas = service
        .brainstorm(&req.topic, req.count)
        .await
        .map_err(|e| ApiError::CompletionsError(e.to_string()))?;

    Ok(Json(BrainstormResponse { ideas }))
}

/// POST /api/completions/summarize — Summarize text.
///
/// Given text, generates a condensed summary.
async fn completions_summarize(
    State(state): State<ApiState>,
    Json(req): Json<SummarizeRequest>,
) -> Result<Json<SummarizeResponse>, ApiError> {
    validate_input_size(&req.text)?;

    let service = state
        .completions_service
        .as_ref()
        .ok_or_else(|| ApiError::CompletionsUnavailable("completions service not configured".into()))?;

    let summary = service
        .summarize(&req.text, req.max_words)
        .await
        .map_err(|e| ApiError::CompletionsError(e.to_string()))?;

    Ok(Json(SummarizeResponse { summary }))
}

// --- Error handling ---

enum ApiError {
    Server(server::ServerError),
    BadRequest(String),
    MergeQueue(String),
    SessionManager(String),
    NotFound(String),
    CompletionsUnavailable(String),
    CompletionsError(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Server(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            ApiError::BadRequest(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::MergeQueue(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::SessionManager(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::NotFound(e) => (StatusCode::NOT_FOUND, e),
            ApiError::CompletionsUnavailable(e) => (StatusCode::SERVICE_UNAVAILABLE, e),
            ApiError::CompletionsError(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
