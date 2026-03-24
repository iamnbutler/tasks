//! Web API — spec Section 3.1, 16.3.
//!
//! HTTP REST API and SSE event stream for the web GUI.
//! The server serves the built frontend as static files and
//! exposes API endpoints under `/api/`.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

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
use server::presence::OwnedConnectionGuard;
use models::Mode;

use crate::update::UpdateState;

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub server: Arc<Server>,
    pub max_sessions: u32,
    pub session_manager: Option<Arc<tasks_session::SessionManager<runtime::AppleContainerRuntime>>>,
    pub completions_service: Option<Arc<CompletionsService>>,
    pub automation_executor: Option<Arc<server::AutomationExecutor>>,
    pub update_state: Arc<UpdateState>,
    pub update_tx: tokio::sync::mpsc::Sender<()>,
}

/// Build the API router.
pub fn router(state: ApiState) -> Router {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}", axum::routing::patch(update_task))
        .route("/tasks/reorder", post(reorder_tasks))
        .route("/tasks/{id}/events", get(get_task_events))
        .route("/tasks/{id}/chat", post(send_chat))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/projects", get(list_projects))
        .route("/projects", post(add_project))
        .route("/projects/bootstrap", post(bootstrap_project))
        .route("/projects/{id}", axum::routing::delete(delete_project))
        .route("/issues", post(create_issue))
        .route("/merge-queue", get(list_merge_queue))
        .route("/merge-queue/flush", post(flush_merge_queue))
        .route("/merge-queue/{id}/approve", post(approve_merge))
        .route("/merge-queue/{id}/reject", post(reject_merge))
        .route("/merge-queue/{id}/request-changes", post(request_changes))
        .route("/mode", get(get_mode))
        .route("/mode", post(set_mode))
        .route("/rebuild", post(rebuild_from_github))
        .route("/orchestrator/chat", post(orchestrator_chat))
        .route("/accounting", get(get_accounting_summary))
        .route("/accounting/tasks", get(list_task_accounting))
        .route("/accounting/tasks/{id}", get(get_task_accounting))
        .route("/containers", get(list_containers))
        .route("/events/query", get(query_events))
        // Self-update endpoints (issue #305, #320)
        .route("/self-update", get(get_self_update_status))
        .route("/self-update/apply", post(apply_self_update))
        .route("/events", get(event_stream))
        // Completions endpoints (fast mode with Haiku)
        .route("/completions", post(completions))
        .route("/completions/name", post(completions_name))
        .route("/completions/describe", post(completions_describe))
        .route("/completions/brainstorm", post(completions_brainstorm))
        .route("/completions/summarize", post(completions_summarize))
        // Automation endpoints (spec §5.7)
        .route("/automations", get(list_automations))
        .route("/automations", post(create_automation))
        .route("/automations/{id}", get(get_automation))
        .route("/automations/{id}", axum::routing::patch(update_automation))
        .route("/automations/{id}", axum::routing::delete(delete_automation))
        .route("/automations/{id}/runs", get(list_automation_runs))
        .route("/automations/{id}/run", post(trigger_automation));

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
    automations: Vec<models::automation::Automation>,
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
struct CreateIssueRequest {
    /// Project ID to create the issue in.
    project_id: String,
    /// Issue title.
    title: String,
    /// Issue body (markdown).
    body: Option<String>,
    /// Labels to add to the issue.
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Serialize)]
struct CreateIssueResponse {
    /// Created issue number.
    number: u64,
    /// Issue URL.
    url: String,
}

#[derive(Deserialize)]
struct BootstrapProjectRequest {
    /// The prompt describing what to build.
    prompt: String,
    /// Optional repository name. If not provided, a name will be derived from the prompt.
    repo_name: Option<String>,
}

#[derive(Serialize)]
struct BootstrapProjectResponse {
    /// The created project.
    project: models::project::Project,
    /// The created GitHub issue.
    issue: BootstrapIssueInfo,
    /// The repository URL.
    repo_url: String,
}

#[derive(Serialize)]
struct BootstrapIssueInfo {
    /// Issue number.
    number: u64,
    /// Issue URL.
    url: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Deserialize)]
struct UpdateTaskRequest {
    /// New priority value. Lower numbers are higher priority.
    priority: Option<i32>,
}

#[derive(Deserialize)]
struct ReorderTasksRequest {
    /// Task IDs in the desired order. Tasks will be assigned
    /// priorities 1, 2, 3, ... in this order.
    task_ids: Vec<String>,
}

#[derive(Deserialize)]
struct OrchestratorChatRequest {
    message: String,
}

#[derive(Deserialize)]
struct RequestChangesRequest {
    /// Reason for requesting changes.
    reasoning: String,
    /// Specific, actionable feedback for the agent.
    feedback: String,
}

#[derive(Deserialize)]
struct EventStreamQuery {
    /// Optional event type pattern filter (e.g. "task:*", "agent:message").
    pattern: Option<String>,
    /// Optional task ID filter.
    task_id: Option<String>,
}

#[derive(Deserialize)]
struct QueryEventsParams {
    /// Event type prefix to filter by (e.g. "orchestrator:").
    type_prefix: Option<String>,
    /// Maximum number of events to return (default: 200).
    limit: Option<usize>,
}

// --- Self-update types (issue #305, #320) ---

/// GET /api/self-update response.
#[derive(Serialize)]
struct SelfUpdateStatusResponse {
    /// Whether an update is available.
    available: bool,
    /// Whether an update is currently being applied.
    applying: bool,
    /// Current commit hash (short form).
    current_commit: Option<String>,
    /// Target commit hash to update to (short form).
    target_commit: Option<String>,
    /// What needs to be rebuilt: "server", "container", or "frontend".
    rebuild_scope: Option<String>,
    /// First line of the commit message for the target commit.
    commit_summary: Option<String>,
    /// When the update was last checked.
    last_checked: Option<chrono::DateTime<chrono::Utc>>,
}

/// POST /api/self-update/apply request.
#[derive(Deserialize)]
struct ApplySelfUpdateRequest {
    /// If true, skip waiting for active sessions to complete.
    #[serde(default)]
    force: bool,
}

/// POST /api/self-update/apply response.
#[derive(Serialize)]
struct ApplySelfUpdateResponse {
    /// Status of the apply request: "applying", "no_update", "already_applying".
    status: String,
    /// Human-readable message about the update status.
    message: String,
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

// --- Automation request/response types (spec §5.7) ---

#[derive(Deserialize)]
struct ListAutomationsQuery {
    /// Optional project_id to filter automations by.
    project_id: Option<String>,
}

#[derive(Deserialize)]
struct CreateAutomationRequest {
    /// Project ID this automation belongs to.
    project_id: String,
    /// Human-readable name.
    name: String,
    /// Prompt template for the automation.
    prompt: String,
    /// Trigger type for the automation.
    trigger: models::automation::TriggerType,
}

#[derive(Deserialize)]
struct UpdateAutomationRequest {
    /// Update the name.
    name: Option<String>,
    /// Update the prompt.
    prompt: Option<String>,
    /// Update the state (active, paused, disabled).
    state: Option<models::automation::AutomationState>,
    /// Update the trigger.
    trigger: Option<models::automation::TriggerType>,
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
        merge_queue: server_state.merge_queue.entries_with_positions(),
        automations: server_state.automations.values().cloned().collect(),
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

/// PATCH /api/tasks/:id — Update a task's properties.
///
/// Currently supports updating priority for manual queue reordering.
async fn update_task(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateTaskRequest>,
) -> Result<Json<models::task::Task>, ApiError> {
    state
        .server
        .set_task_priority(&id, req.priority, Actor::Human)
        .await
        .map_err(ApiError::Server)?;

    let server_state = state.server.state.read().await;
    server_state
        .tasks
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("task not found: {}", id)))
}

/// POST /api/tasks/reorder — Reorder tasks by assigning sequential priorities.
///
/// Takes a list of task IDs in the desired order. Tasks will be assigned
/// priorities 1, 2, 3, ... in that order. This is used for drag-and-drop
/// reordering in the GUI.
async fn reorder_tasks(
    State(state): State<ApiState>,
    Json(req): Json<ReorderTasksRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .server
        .reorder_tasks(&req.task_ids, Actor::Human)
        .await
        .map_err(ApiError::Server)?;
    Ok(StatusCode::OK)
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

/// POST /api/projects/bootstrap — Create a new project from scratch.
///
/// Creates a new private GitHub repository, adds it as a project, and creates
/// an initial issue with the prompt. The poller will pick up the issue and
/// dispatch an agent to work on it.
async fn bootstrap_project(
    State(state): State<ApiState>,
    Json(req): Json<BootstrapProjectRequest>,
) -> Result<Json<BootstrapProjectResponse>, ApiError> {
    // Validate prompt
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(ApiError::BadRequest("prompt cannot be empty".to_string()));
    }

    // Get GitHub token
    let github_token = std::env::var("GITHUB_TOKEN")
        .map_err(|_| ApiError::Config("GITHUB_TOKEN not configured on server".to_string()))?;

    let client = tasks_github::GitHubClient::new(&github_token);

    // Determine repository name
    let repo_name = if let Some(name) = req.repo_name {
        let name = name.trim();
        if name.is_empty() {
            return Err(ApiError::BadRequest("repo_name cannot be empty if provided".to_string()));
        }
        // Sanitize: replace spaces with hyphens, lowercase, remove non-alphanumeric
        sanitize_repo_name(name)
    } else {
        // Derive from prompt: take first few words, sanitize
        derive_repo_name(prompt)
    };

    // Create the repository
    let description_text;
    let description = if prompt.chars().count() > 200 {
        description_text = prompt.chars().take(200).collect::<String>();
        Some(description_text.as_str())
    } else {
        Some(prompt)
    };

    let created_repo = client
        .create_repository(&repo_name, description)
        .await
        .map_err(ApiError::GitHubApi)?;

    // Add the project to tracking
    let project_id = uuid::Uuid::new_v4().to_string();
    let project = models::project::Project::new(&project_id, &created_repo.full_name);
    state.server.add_project(project.clone()).await;

    // Parse owner/repo from the created repository
    let parts: Vec<&str> = created_repo.full_name.split('/').collect();
    let (owner, repo) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        return Err(ApiError::BadRequest(format!(
            "unexpected GitHub full_name format: {}",
            created_repo.full_name
        )));
    };

    // Create the initial issue with the prompt
    let issue_title = derive_issue_title(prompt);
    let issue_body = format!(
        "## What to build\n\n{}\n\n---\n\n*This project was bootstrapped automatically. \
        Create additional issues for questions or clarifications.*",
        prompt
    );

    let created_issue = client
        .create_issue(owner, repo, &issue_title, Some(&issue_body), None)
        .await
        .map_err(ApiError::GitHubApi)?;

    Ok(Json(BootstrapProjectResponse {
        project,
        issue: BootstrapIssueInfo {
            number: created_issue.number,
            url: created_issue.html_url,
        },
        repo_url: created_repo.html_url,
    }))
}

/// Sanitize a user-provided repository name.
fn sanitize_repo_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "new-project".to_string()
    } else {
        sanitized
    }
}

/// Derive a repository name from a prompt.
fn derive_repo_name(prompt: &str) -> String {
    // Take the first ~5 meaningful words
    let words: Vec<&str> = prompt
        .split_whitespace()
        .filter(|w| w.len() > 2) // Skip short words like "a", "an", "to"
        .take(5)
        .collect();

    let name = if words.is_empty() {
        "new-project".to_string()
    } else {
        words.join("-")
    };

    sanitize_repo_name(&name)
}

/// Derive an issue title from a prompt.
fn derive_issue_title(prompt: &str) -> String {
    // Take the first line or first ~60 chars
    let first_line = prompt.lines().next().unwrap_or(prompt);
    if first_line.len() <= 60 {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(57).collect();
        format!("{truncated}...")
    }
}

/// POST /api/issues — Create a new GitHub issue.
///
/// Creates an issue in the GitHub repository associated with the given project.
/// The poller will pick up the new issue on its next cycle and create a task from it.
async fn create_issue(
    State(state): State<ApiState>,
    Json(req): Json<CreateIssueRequest>,
) -> Result<Json<CreateIssueResponse>, ApiError> {
    // Validate title
    if req.title.trim().is_empty() {
        return Err(ApiError::BadRequest("title cannot be empty".to_string()));
    }

    // Look up the project to get owner/repo
    let (owner, repo) = {
        let server_state = state.server.state.read().await;
        let project = server_state
            .projects
            .get(&req.project_id)
            .ok_or_else(|| ApiError::NotFound(format!("project not found: {}", req.project_id)))?;

        // Parse owner/repo from project.repo
        let parts: Vec<&str> = project.repo.split('/').collect();
        if parts.len() != 2 {
            return Err(ApiError::BadRequest(format!(
                "invalid repo format in project: {}",
                project.repo
            )));
        }
        (parts[0].to_string(), parts[1].to_string())
    };

    // Create GitHub client and issue
    let github_token = std::env::var("GITHUB_TOKEN")
        .map_err(|_| ApiError::Config("GITHUB_TOKEN not configured on server".to_string()))?;

    let client = tasks_github::GitHubClient::new(&github_token);

    let labels: Option<&[String]> = if req.labels.is_empty() {
        None
    } else {
        Some(&req.labels)
    };

    let created = client
        .create_issue(&owner, &repo, &req.title, req.body.as_deref(), labels)
        .await
        .map_err(ApiError::GitHubApi)?;

    Ok(Json(CreateIssueResponse {
        number: created.number,
        url: created.html_url,
    }))
}

/// GET /api/merge-queue — List merge queue entries.
async fn list_merge_queue(
    State(state): State<ApiState>,
) -> Json<Vec<models::merge_queue::MergeQueueEntry>> {
    let server_state = state.server.state.read().await;
    Json(server_state.merge_queue.entries_with_positions())
}

/// POST /api/merge-queue/flush — Flush approved entries (Pause mode only).
///
/// Collects approved entries and executes the actual GitHub merges.
/// Each entry's status is updated based on the merge result:
/// - Success: marked as Merged, task transitions to Completed
/// - Failure: marked as Conflict
async fn flush_merge_queue(State(state): State<ApiState>) -> Result<Json<Vec<String>>, ApiError> {
    // Collect approved entries (mode check happens here - Pause mode only)
    let entries_to_merge = state
        .server
        .collect_entries_for_flush()
        .await
        .map_err(ApiError::Server)?;

    if entries_to_merge.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let mut merged_ids: Vec<String> = Vec::new();

    // Execute GitHub merges for each entry
    let github_token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if github_token.is_empty() {
        tracing::warn!("GITHUB_TOKEN not set, cannot perform merges");
        return Ok(Json(Vec::new()));
    }

    let client = tasks_github::client::GitHubClient::new(&github_token);
    for (entry_id, pr_url) in &entries_to_merge {
        if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(pr_url) {
            // Transition to Merging before the API call for visibility
            if let Err(e) = state.server.mark_entry_merging(entry_id, pr_url).await {
                tracing::error!(entry_id = %entry_id, error = %e, "failed to mark entry as merging");
            }

            match client.merge_pull_request(&owner, &repo, number).await {
                Ok(true) => {
                    tracing::info!(entry_id = %entry_id, pr_url = %pr_url, "PR merged via flush");
                    if let Err(e) = state.server.mark_entry_merged(entry_id, pr_url).await {
                        tracing::error!(entry_id = %entry_id, error = %e, "failed to mark entry merged after flush");
                    } else {
                        merged_ids.push(entry_id.clone());
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

    // Emit flush event with the IDs that were successfully merged
    if !merged_ids.is_empty() {
        if let Err(e) = state.server.emit_flush_event(&merged_ids).await {
            tracing::error!(error = %e, "failed to emit flush event");
        }
    }

    Ok(Json(merged_ids))
}

/// POST /api/merge-queue/:id/approve — Approve a merge queue entry.
///
/// In Play mode, this also triggers an immediate merge via the GitHub API.
/// In Pause mode, the entry is approved but merge happens on flush.
async fn approve_merge(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Get entry details and current mode before modifying state
    let (pr_url, mode) = {
        let server_state = state.server.state.read().await;
        let entry = server_state
            .merge_queue
            .get(&id)
            .ok_or_else(|| ApiError::MergeQueue(format!("entry not found: {}", id)))?;
        (entry.pr_url.clone(), server_state.mode)
    };

    // Approve the entry using the server method (emits merge:approved event)
    state
        .server
        .approve_merge_entry(&id, "Manual approval via API")
        .await
        .map_err(ApiError::Server)?;

    // In Play mode, trigger immediate merge (Play mode = continuous merge authority)
    if mode == Mode::Play {
        let github_token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
        if !github_token.is_empty() {
            if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(&pr_url) {
                // Transition to Merging before the API call for visibility
                if let Err(e) = state.server.mark_entry_merging(&id, &pr_url).await {
                    tracing::error!(entry_id = %id, error = %e, "failed to mark entry as merging");
                }

                let client = tasks_github::client::GitHubClient::new(&github_token);
                match client.merge_pull_request(&owner, &repo, number).await {
                    Ok(true) => {
                        tracing::info!(entry_id = %id, pr_url = %pr_url, "PR merged after manual approval (Play mode)");
                        if let Err(e) = state.server.mark_entry_merged(&id, &pr_url).await {
                            tracing::error!(entry_id = %id, error = %e, "failed to mark entry merged");
                        }
                    }
                    Ok(false) => {
                        tracing::warn!(entry_id = %id, pr_url = %pr_url, "PR not mergeable after approval");
                        if let Err(e) = state.server.mark_entry_conflict(&id, &pr_url, None).await {
                            tracing::error!(entry_id = %id, error = %e, "failed to mark entry conflict");
                        }
                    }
                    Err(e) => {
                        tracing::error!(entry_id = %id, pr_url = %pr_url, error = %e, "failed to merge PR after approval");
                    }
                }
            }
        } else {
            tracing::warn!("GITHUB_TOKEN not set, cannot perform merge in Play mode");
        }
    }

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

/// POST /api/merge-queue/:id/request-changes — Request changes on a merge queue entry.
///
/// Unlike rejection, the entry stays in the queue and the task gets priority dispatch.
async fn request_changes(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RequestChangesRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .server
        .request_changes_merge_entry(&id, &req.reasoning, &req.feedback)
        .await
        .map_err(ApiError::Server)?;
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
/// When transitioning to Stop mode, session termination is handled by an
/// event-driven listener in the run loop (spec §6.1).
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

/// POST /api/rebuild — Rebuild state from GitHub (issue #256).
///
/// Clears tasks and merge queue from both memory and database,
/// then signals the poll loop to re-fetch all data from GitHub.
/// Preserves: accounting data, event logs, projects table, operating mode.
///
/// Note: The response contains counts of items *cleared*. The actual re-fetch
/// happens asynchronously in the poll loop — new data will appear as the
/// pollers discover items from GitHub.
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

// --- Containers endpoint ---

/// GET /api/containers — List all active container sessions.
///
/// Returns information about each running container including its ID, associated
/// task, start time, and uptime. Used for the containers monitoring view.
async fn list_containers(
    State(state): State<ApiState>,
) -> Json<Vec<tasks_session::ContainerInfo>> {
    match &state.session_manager {
        Some(session_manager) => {
            let containers = session_manager.container_info().await;
            Json(containers)
        }
        None => Json(vec![]),
    }
}

// --- Self-update endpoints (issue #305, #320) ---

/// GET /api/self-update — Get current update status.
///
/// Returns information about whether an update is available from upstream.
/// Note: Full functionality requires Phase 1 infrastructure (#319).
async fn get_self_update_status(
    State(state): State<ApiState>,
) -> Json<SelfUpdateStatusResponse> {
    let applying = state.update_state.is_applying();
    match state.update_state.get_info().await {
        Some(info) => Json(SelfUpdateStatusResponse {
            available: true,
            applying,
            current_commit: Some(info.current_commit[..7.min(info.current_commit.len())].to_string()),
            target_commit: Some(info.available_commit[..7.min(info.available_commit.len())].to_string()),
            rebuild_scope: Some(info.scope.as_str().to_string()),
            commit_summary: None,
            last_checked: None,
        }),
        None => Json(SelfUpdateStatusResponse {
            available: false,
            applying,
            current_commit: None,
            target_commit: None,
            rebuild_scope: None,
            commit_summary: None,
            last_checked: None,
        }),
    }
}

/// POST /api/self-update/apply — Trigger a self-update.
///
/// Initiates the update process:
/// 1. Sets mode to Stop to prevent new session launches
/// 2. Waits for active sessions to complete (unless force=true)
/// 3. Exits with code 100 for the wrapper script to restart
///
/// Note: Full functionality requires Phase 1 infrastructure (#319).
async fn apply_self_update(
    State(state): State<ApiState>,
    Json(req): Json<ApplySelfUpdateRequest>,
) -> Result<Json<ApplySelfUpdateResponse>, ApiError> {
    // Already applying?
    if state.update_state.is_applying() {
        return Ok(Json(ApplySelfUpdateResponse {
            status: "already_applying".to_string(),
            message: "An update is already being applied.".to_string(),
        }));
    }

    // No update available?
    if !state.update_state.is_available().await {
        return Ok(Json(ApplySelfUpdateResponse {
            status: "no_update".to_string(),
            message: "No update available.".to_string(),
        }));
    }

    tracing::info!(force = req.force, "self-update apply triggered via API");

    // Send the update trigger — the run loop will handle shutdown and exit 100
    if let Err(e) = state.update_tx.send(()).await {
        tracing::error!(error = %e, "failed to send update trigger");
        return Err(ApiError::BadRequest(format!("failed to trigger update: {e}")));
    }

    Ok(Json(ApplySelfUpdateResponse {
        status: "applying".to_string(),
        message: "Update is being applied. The server will restart shortly.".to_string(),
    }))
}

/// A stream wrapper that holds a presence guard until the stream ends.
///
/// This ensures the human presence is tracked for the entire duration
/// of an SSE connection, not just when the handler function returns.
#[pin_project::pin_project]
struct PresenceStream<S> {
    #[pin]
    inner: S,
    #[allow(dead_code)]
    guard: OwnedConnectionGuard,
}

impl<S: Stream> Stream for PresenceStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

/// GET /api/events/query — Query historical events by type prefix.
///
/// Returns events matching the given `type_prefix` across all task logs,
/// sorted by timestamp ascending and capped at `limit` (default 200).
async fn query_events(
    State(state): State<ApiState>,
    Query(params): Query<QueryEventsParams>,
) -> Result<Json<Vec<events::Event>>, ApiError> {
    let type_prefix = params.type_prefix.unwrap_or_default();
    let limit = params.limit.unwrap_or(200);

    if type_prefix.is_empty() {
        return Err(ApiError::BadRequest(
            "type_prefix query parameter is required".to_string(),
        ));
    }

    state
        .server
        .event_bus
        .query_by_type_prefix(&type_prefix, limit)
        .await
        .map(Json)
        .map_err(|e| ApiError::Server(server::ServerError::EventStore(e)))
}

/// GET /api/events — SSE stream of live events.
///
/// Supports optional query params: `pattern` and `task_id` for filtering.
async fn event_stream(
    State(state): State<ApiState>,
    Query(query): Query<EventStreamQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // Register presence — the guard lives until the stream is dropped (client disconnects).
    let presence_guard = state.server.presence.connect_owned();

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

    // Wrap the stream to keep the presence guard alive until disconnection.
    let presence_stream = PresenceStream {
        inner: stream,
        guard: presence_guard,
    };

    Sse::new(presence_stream).keep_alive(KeepAlive::default())
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

// --- Automation handlers (spec §5.7) ---

/// GET /api/automations — List automations (optionally filtered by project_id).
async fn list_automations(
    State(state): State<ApiState>,
    Query(query): Query<ListAutomationsQuery>,
) -> Json<Vec<models::automation::Automation>> {
    let server_state = state.server.state.read().await;
    let automations: Vec<_> = if let Some(ref project_id) = query.project_id {
        server_state
            .automations
            .values()
            .filter(|a| &a.project_id == project_id)
            .cloned()
            .collect()
    } else {
        server_state.automations.values().cloned().collect()
    };
    Json(automations)
}

/// POST /api/automations — Create a new automation.
async fn create_automation(
    State(state): State<ApiState>,
    Json(req): Json<CreateAutomationRequest>,
) -> Result<Json<models::automation::Automation>, ApiError> {
    // Validate name and prompt are not empty
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name cannot be empty".to_string()));
    }
    if req.prompt.trim().is_empty() {
        return Err(ApiError::BadRequest("prompt cannot be empty".to_string()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let automation = models::automation::Automation::new(&id, &req.project_id, &req.name, &req.prompt, req.trigger);

    state
        .server
        .add_automation(automation.clone())
        .await
        .map_err(|e| match e {
            server::ServerError::ProjectNotFound(_) => ApiError::NotFound(e.to_string()),
            _ => ApiError::Server(e),
        })?;

    Ok(Json(automation))
}

/// GET /api/automations/:id — Get a single automation.
async fn get_automation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<models::automation::Automation>, ApiError> {
    let server_state = state.server.state.read().await;
    server_state
        .automations
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("automation not found: {}", id)))
}

/// PATCH /api/automations/:id — Update an automation.
async fn update_automation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAutomationRequest>,
) -> Result<Json<models::automation::Automation>, ApiError> {
    // Validate name if provided
    if let Some(ref name) = req.name {
        if name.trim().is_empty() {
            return Err(ApiError::BadRequest("name cannot be empty".to_string()));
        }
    }
    // Validate prompt if provided
    if let Some(ref prompt) = req.prompt {
        if prompt.trim().is_empty() {
            return Err(ApiError::BadRequest("prompt cannot be empty".to_string()));
        }
    }

    let automation = state
        .server
        .update_automation(&id, req.name, req.prompt, req.state, req.trigger)
        .await
        .map_err(|e| match e {
            server::ServerError::StoreError(ref msg) if msg.contains("not found") => {
                ApiError::NotFound(e.to_string())
            }
            _ => ApiError::Server(e),
        })?;

    Ok(Json(automation))
}

/// DELETE /api/automations/:id — Delete an automation.
async fn delete_automation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = state.server.remove_automation(&id).await;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound(format!("automation not found: {}", id)))
    }
}

/// GET /api/automations/:id/runs — List runs for an automation.
async fn list_automation_runs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<models::automation::AutomationRun>>, ApiError> {
    // Verify automation exists
    {
        let server_state = state.server.state.read().await;
        if !server_state.automations.contains_key(&id) {
            return Err(ApiError::NotFound(format!("automation not found: {}", id)));
        }
    }

    let runs = state
        .server
        .list_automation_runs(&id)
        .map_err(ApiError::Server)?;

    Ok(Json(runs))
}

/// POST /api/automations/:id/run — Manually trigger an automation run.
async fn trigger_automation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<models::automation::AutomationRun>, ApiError> {
    // Get the automation first to validate it exists
    let automation = state
        .server
        .get_automation(&id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("automation not found: {}", id)))?;

    // Get the project for the automation
    let project = state
        .server
        .get_project(&automation.project_id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("project not found: {}", automation.project_id)))?;

    // Create the run
    let run = state
        .server
        .create_automation_run(&id)
        .await
        .map_err(|e| match e {
            server::ServerError::StoreError(ref msg) if msg.contains("not found") => {
                ApiError::NotFound(e.to_string())
            }
            _ => ApiError::Server(e),
        })?;

    // If executor is available, spawn the execution in background
    if let Some(executor) = &state.automation_executor {
        let run_id = run.id.clone();
        let automation_id = automation.id.clone();
        let server = state.server.clone();
        let executor = executor.clone();
        let event_bus = server.event_bus.clone();
        let context = server::ExecutionContext::default();

        tokio::spawn(async move {
            // Use streaming execution to emit output events in real-time
            let result = executor.execute_streaming(
                &automation,
                &project,
                &context,
                |chunk| {
                    // Emit output event for each chunk (fire-and-forget)
                    let event = events::Event::new(
                        events::EventType::AutomationRunOutput,
                        &run_id,
                        Actor::System,
                        serde_json::json!({
                            "automation_id": &automation_id,
                            "chunk": chunk,
                        }),
                    );
                    // Try to publish but don't block on it
                    let bus = event_bus.clone();
                    let _ = tokio::spawn(async move {
                        let _ = bus.publish(event).await;
                    });
                },
            ).await;

            match result {
                Ok(exec_result) => {
                    if let Err(e) = server.complete_automation_run(&run_id, Some(exec_result.output)).await {
                        tracing::error!(run_id = %run_id, error = %e, "Failed to complete automation run");
                    }
                }
                Err(e) => {
                    if let Err(e2) = server.fail_automation_run(&run_id, e.to_string()).await {
                        tracing::error!(run_id = %run_id, error = %e2, "Failed to mark automation run as failed");
                    }
                }
            }
        });
    } else {
        // No executor available, fail the run immediately
        let run_id = run.id.clone();
        let server = state.server.clone();
        tokio::spawn(async move {
            if let Err(e) = server.fail_automation_run(
                &run_id,
                "Automation executor not available (ANTHROPIC_API_KEY not set)",
            ).await {
                tracing::error!(run_id = %run_id, error = %e, "Failed to mark automation run as failed");
            }
        });
    }

    Ok(Json(run))
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
    /// Configuration error (e.g., missing GITHUB_TOKEN).
    Config(String),
    /// GitHub API error with proper status code mapping.
    GitHubApi(tasks_github::GitHubError),
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
            ApiError::Config(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),
            ApiError::GitHubApi(e) => {
                use tasks_github::GitHubError;
                let (status, msg) = match &e {
                    GitHubError::Auth(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
                    GitHubError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
                    GitHubError::RateLimited { reset_at } => (
                        StatusCode::TOO_MANY_REQUESTS,
                        format!("GitHub rate limit exceeded, resets at {reset_at}"),
                    ),
                    GitHubError::Validation(msg) => (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        format!("GitHub validation error: {msg}"),
                    ),
                    GitHubError::GraphQL(errors) => {
                        let msgs: Vec<_> = errors.iter().map(|e| e.message.clone()).collect();
                        (StatusCode::BAD_GATEWAY, format!("GitHub API errors: {}", msgs.join(", ")))
                    }
                    GitHubError::Network(_) => (
                        StatusCode::BAD_GATEWAY,
                        format!("GitHub network error: {e}"),
                    ),
                    GitHubError::Decode(msg) => (
                        StatusCode::BAD_GATEWAY,
                        format!("GitHub response decode error: {msg}"),
                    ),
                };
                (status, msg)
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
