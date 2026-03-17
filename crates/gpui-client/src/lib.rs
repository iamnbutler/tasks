//! HTTP API client for the Tasks platform GPUI frontend.
//!
//! This crate provides a typed client for communicating with the Tasks backend API.
//! It mirrors the endpoints defined in the web frontend (`web/src/lib/api.ts`).

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// Re-export types from internal crates for convenience
pub use events::Event;
pub use models::merge_queue::{MergeQueueEntry, MergeStatus};
pub use models::project::Project;
pub use models::task::{Task, TaskSource, TaskState};
pub use models::Mode;

/// API client errors.
#[derive(Debug, Error)]
pub enum ApiError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Server returned an error response.
    #[error("Server error ({status}): {message}")]
    Server { status: u16, message: String },
}

/// Slot utilization information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotUtilization {
    /// Number of active sessions.
    pub active: u32,
    /// Maximum concurrent sessions.
    pub max: u32,
}

/// Full system state snapshot (spec Section 16.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub mode: Mode,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub merge_queue: Vec<MergeQueueEntry>,
    pub slot_utilization: SlotUtilization,
    pub human_present: bool,
}

/// Response from mode endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeResponse {
    pub mode: Mode,
}

/// Request body for setting the operating mode.
#[derive(Debug, Clone, Serialize)]
struct SetModeRequest {
    mode: Mode,
}

/// Request body for adding a project.
#[derive(Debug, Clone, Serialize)]
struct AddProjectRequest {
    repo: String,
}

/// Request body for sending a chat message.
#[derive(Debug, Clone, Serialize)]
struct ChatRequest {
    message: String,
}

/// HTTP API client for the Tasks backend.
///
/// All methods are async and return `Result<T, ApiError>`.
#[derive(Debug, Clone)]
pub struct ApiClient {
    client: Client,
    base_url: String,
}

impl ApiClient {
    /// Create a new API client.
    ///
    /// The `base_url` should be the root URL of the Tasks server (e.g., `http://localhost:4800`).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Create a new API client with a custom reqwest Client.
    pub fn with_client(client: Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api{}", self.base_url.trim_end_matches('/'), path)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let response = self.client.get(self.url(path)).send().await?;
        self.handle_response(response).await
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ApiError> {
        let response = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await?;
        self.handle_response(response).await
    }

    async fn post_empty(&self, path: &str) -> Result<(), ApiError> {
        let response = self.client.post(self.url(path)).send().await?;
        self.handle_empty_response(response).await
    }

    async fn post_with_body_empty(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<(), ApiError> {
        let response = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await?;
        self.handle_empty_response(response).await
    }

    async fn delete(&self, path: &str) -> Result<(), ApiError> {
        let response = self.client.delete(self.url(path)).send().await?;
        self.handle_empty_response(response).await
    }

    async fn handle_response<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, ApiError> {
        let status = response.status();
        if status.is_success() {
            Ok(response.json().await?)
        } else {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            Err(ApiError::Server {
                status: status.as_u16(),
                message,
            })
        }
    }

    async fn handle_empty_response(&self, response: reqwest::Response) -> Result<(), ApiError> {
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            Err(ApiError::Server {
                status: status.as_u16(),
                message,
            })
        }
    }

    // --- Snapshot ---

    /// Fetch the full system state snapshot.
    ///
    /// GET /api/snapshot
    pub async fn fetch_snapshot(&self) -> Result<Snapshot, ApiError> {
        self.get("/snapshot").await
    }

    // --- Tasks ---

    /// Fetch all tasks.
    ///
    /// GET /api/tasks
    pub async fn fetch_tasks(&self) -> Result<Vec<Task>, ApiError> {
        self.get("/tasks").await
    }

    /// Fetch a single task by ID.
    ///
    /// GET /api/tasks/:id
    pub async fn fetch_task(&self, id: &str) -> Result<Task, ApiError> {
        self.get(&format!("/tasks/{}", id)).await
    }

    /// Fetch event history for a task.
    ///
    /// GET /api/tasks/:id/events
    pub async fn fetch_task_events(&self, id: &str) -> Result<Vec<Event>, ApiError> {
        self.get(&format!("/tasks/{}/events", id)).await
    }

    /// Send a chat message to a running agent session.
    ///
    /// POST /api/tasks/:id/chat
    pub async fn send_chat(&self, task_id: &str, message: impl Into<String>) -> Result<(), ApiError> {
        self.post_with_body_empty(
            &format!("/tasks/{}/chat", task_id),
            &ChatRequest {
                message: message.into(),
            },
        )
        .await
    }

    /// Cancel a running task.
    ///
    /// POST /api/tasks/:id/cancel
    pub async fn cancel_task(&self, task_id: &str) -> Result<(), ApiError> {
        self.post_empty(&format!("/tasks/{}/cancel", task_id)).await
    }

    // --- Projects ---

    /// Fetch all projects.
    ///
    /// GET /api/projects
    pub async fn fetch_projects(&self) -> Result<Vec<Project>, ApiError> {
        self.get("/projects").await
    }

    /// Add a new project.
    ///
    /// POST /api/projects
    pub async fn add_project(&self, repo: impl Into<String>) -> Result<Project, ApiError> {
        self.post("/projects", &AddProjectRequest { repo: repo.into() })
            .await
    }

    /// Delete a project by ID.
    ///
    /// DELETE /api/projects/:id
    pub async fn delete_project(&self, id: &str) -> Result<(), ApiError> {
        self.delete(&format!("/projects/{}", id)).await
    }

    // --- Merge Queue ---

    /// Fetch all merge queue entries.
    ///
    /// GET /api/merge-queue
    pub async fn fetch_merge_queue(&self) -> Result<Vec<MergeQueueEntry>, ApiError> {
        self.get("/merge-queue").await
    }

    /// Approve a merge queue entry.
    ///
    /// POST /api/merge-queue/:id/approve
    pub async fn approve_merge(&self, id: &str) -> Result<(), ApiError> {
        self.post_empty(&format!("/merge-queue/{}/approve", id))
            .await
    }

    /// Reject a merge queue entry.
    ///
    /// POST /api/merge-queue/:id/reject
    pub async fn reject_merge(&self, id: &str) -> Result<(), ApiError> {
        self.post_empty(&format!("/merge-queue/{}/reject", id))
            .await
    }

    /// Flush the merge queue (Pause mode only).
    ///
    /// Returns the IDs of flushed entries.
    ///
    /// POST /api/merge-queue/flush
    pub async fn flush_merge_queue(&self) -> Result<(), ApiError> {
        self.post_empty("/merge-queue/flush").await
    }

    // --- Mode ---

    /// Get the current operating mode.
    ///
    /// GET /api/mode
    pub async fn fetch_mode(&self) -> Result<ModeResponse, ApiError> {
        self.get("/mode").await
    }

    /// Set the operating mode.
    ///
    /// POST /api/mode
    pub async fn set_mode(&self, mode: Mode) -> Result<ModeResponse, ApiError> {
        self.post("/mode", &SetModeRequest { mode }).await
    }

    // --- Orchestrator ---

    /// Send a message to the orchestrator.
    ///
    /// POST /api/orchestrator/chat
    pub async fn send_orchestrator_chat(&self, message: impl Into<String>) -> Result<(), ApiError> {
        self.post_with_body_empty(
            "/orchestrator/chat",
            &ChatRequest {
                message: message.into(),
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_url_construction() {
        let client = ApiClient::new("http://localhost:4800");
        assert_eq!(client.url("/snapshot"), "http://localhost:4800/api/snapshot");
        assert_eq!(client.url("/tasks/123"), "http://localhost:4800/api/tasks/123");
    }

    #[test]
    fn client_url_trailing_slash_stripped() {
        let client = ApiClient::new("http://localhost:4800/");
        // Trailing slash in base_url should be stripped to avoid double-slash
        assert_eq!(client.url("/snapshot"), "http://localhost:4800/api/snapshot");
    }
}
