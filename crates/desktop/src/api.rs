//! HTTP API client for the Tasks server.
//!
//! This module provides type-safe access to the Tasks REST API endpoints.
//! It mirrors the TypeScript API client in `web/src/lib/api.ts`.
//!
//! Domain types (`Task`, `Project`, `MergeQueueEntry`, etc.) are re-exported
//! from the canonical `models` and `events` crates rather than duplicated here.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// Re-export canonical domain types so consumers can import from one place.
pub use events::{Actor, Event};
pub use models::merge_queue::{MergeQueueEntry, MergeStatus};
pub use models::project::Project;
pub use models::task::{Task, TaskSource, TaskState};
pub use models::Mode;

/// Default server URL.
pub const DEFAULT_SERVER_URL: &str = "http://localhost:4800";

// =============================================================================
// Display helpers for canonical types (UI-only, not on the model crate)
// =============================================================================

/// Display-friendly name for a `TaskState`.
pub fn task_state_display_name(state: &TaskState) -> &'static str {
    match state {
        TaskState::Waiting => "Waiting",
        TaskState::Blocked => "Blocked",
        TaskState::Running => "Running",
        TaskState::Question => "Question",
        TaskState::Testing => "Testing",
        TaskState::AwaitingMerge => "Changes Submitted",
        TaskState::Conflict => "Conflict",
        TaskState::ChangesRequested => "Changes Requested",
        TaskState::Completed => "Completed",
        TaskState::Failed => "Failed",
        TaskState::Cancelled => "Cancelled",
    }
}

/// Whether the task is actively consuming an agent slot.
///
/// Matches the server definition: Running, Question, Testing.
/// "Changes Submitted" (AwaitingMerge state) is NOT active — the agent
/// has finished and the PR is waiting in the merge queue.
pub fn task_state_is_active(state: &TaskState) -> bool {
    matches!(
        state,
        TaskState::Running | TaskState::Question | TaskState::Testing
    )
}

/// Display-friendly name for a `Mode`.
pub fn mode_display_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::Stop => "Stop",
        Mode::Pause => "Pause",
        Mode::Play => "Play",
    }
}

/// Display-friendly name for a `MergeStatus`.
pub fn merge_status_display_name(status: &MergeStatus) -> &'static str {
    match status {
        MergeStatus::Pending => "Pending",
        MergeStatus::Approved => "Approved",
        MergeStatus::Merging => "Merging",
        MergeStatus::Rejected => "Rejected",
        MergeStatus::Merged => "Merged",
        MergeStatus::Conflict => "Conflict",
        MergeStatus::ChangesRequested => "Changes Requested",
    }
}

/// Display-friendly label for a `TaskSource`.
pub fn task_source_label(source: &TaskSource) -> String {
    match source {
        TaskSource::GithubIssue {
            owner,
            repo,
            number,
        } => {
            format!("{}/{}#{}", owner, repo, number)
        }
        TaskSource::GithubPr {
            owner,
            repo,
            number,
        } => {
            format!("{}/{}#{} (PR)", owner, repo, number)
        }
        TaskSource::Internal => "Internal".to_string(),
    }
}

/// Display-friendly name for an `Actor`.
pub fn actor_display_name(actor: &Actor) -> &'static str {
    match actor {
        Actor::Human => "Human",
        Actor::Orchestrator => "Orchestrator",
        Actor::Scheduler => "Scheduler",
        Actor::Agent => "Agent",
        Actor::System => "System",
    }
}

// =============================================================================
// API Client
// =============================================================================

/// API client for the Tasks server.
#[derive(Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    base_url: String,
}

impl ApiClient {
    /// Create a new API client with the given base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        let _guard = crate::tokio_runtime().enter();
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Create a new API client with the default server URL.
    pub fn default_url() -> Self {
        Self::new(DEFAULT_SERVER_URL)
    }

    /// Fetch the full system snapshot.
    pub async fn fetch_snapshot(&self) -> Result<Snapshot, ApiError> {
        let url = format!("{}/api/snapshot", self.base_url);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Fetch all tasks.
    pub async fn fetch_tasks(&self) -> Result<Vec<Task>, ApiError> {
        let url = format!("{}/api/tasks", self.base_url);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Fetch a single task by ID.
    pub async fn fetch_task(&self, id: &str) -> Result<Task, ApiError> {
        let url = format!("{}/api/tasks/{}", self.base_url, id);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Fetch events for a specific task.
    pub async fn fetch_task_events(&self, id: &str) -> Result<Vec<Event>, ApiError> {
        let url = format!("{}/api/tasks/{}/events", self.base_url, id);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Fetch all projects.
    pub async fn fetch_projects(&self) -> Result<Vec<Project>, ApiError> {
        let url = format!("{}/api/projects", self.base_url);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Fetch the merge queue.
    pub async fn fetch_merge_queue(&self) -> Result<Vec<MergeQueueEntry>, ApiError> {
        let url = format!("{}/api/merge-queue", self.base_url);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Fetch the current operating mode.
    pub async fn fetch_mode(&self) -> Result<Mode, ApiError> {
        let url = format!("{}/api/mode", self.base_url);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        let mode_response: ModeResponse = response.json().await?;
        Ok(mode_response.mode)
    }

    /// Set the operating mode.
    pub async fn set_mode(&self, mode: Mode) -> Result<Mode, ApiError> {
        let url = format!("{}/api/mode", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&SetModeRequest { mode })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        let mode_response: ModeResponse = response.json().await?;
        Ok(mode_response.mode)
    }

    /// Approve a merge queue entry.
    pub async fn approve_merge(&self, id: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/merge-queue/{}/approve", self.base_url, id);
        let response = self.client.post(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    /// Reject a merge queue entry.
    pub async fn reject_merge(&self, id: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/merge-queue/{}/reject", self.base_url, id);
        let response = self.client.post(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    /// Flush the merge queue (Pause mode only).
    pub async fn flush_merge_queue(&self) -> Result<Vec<String>, ApiError> {
        let url = format!("{}/api/merge-queue/flush", self.base_url);
        let response = self.client.post(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Add a new project.
    pub async fn add_project(&self, repo: &str) -> Result<Project, ApiError> {
        let url = format!("{}/api/projects", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&AddProjectRequest {
                repo: repo.to_string(),
            })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(response.json().await?)
    }

    /// Delete a project.
    pub async fn delete_project(&self, id: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/projects/{}", self.base_url, id);
        let response = self.client.delete(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    /// Send a chat message to a task's agent session.
    pub async fn send_chat(&self, task_id: &str, message: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/tasks/{}/chat", self.base_url, task_id);
        let response = self
            .client
            .post(&url)
            .json(&ChatRequest {
                message: message.to_string(),
            })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    /// Cancel a running task.
    pub async fn cancel_task(&self, task_id: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/tasks/{}/cancel", self.base_url, task_id);
        let response = self.client.post(&url).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }

    /// Send a message to the orchestrator.
    pub async fn send_orchestrator_chat(&self, message: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/orchestrator/chat", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&ChatRequest {
                message: message.to_string(),
            })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ApiError::HttpStatus(response.status().as_u16()));
        }
        Ok(())
    }
}

// =============================================================================
// API-only types (not in canonical model crates)
// =============================================================================

/// Slot utilization info (only returned in snapshot responses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotUtilization {
    pub active: u32,
    pub max: u32,
}

/// Full system snapshot (spec Section 16.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub mode: Mode,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub merge_queue: Vec<MergeQueueEntry>,
    pub slot_utilization: SlotUtilization,
    pub human_present: bool,
}

// --- Request/Response types (internal to API calls) ---

#[derive(Serialize)]
struct SetModeRequest {
    mode: Mode,
}

#[derive(Deserialize)]
struct ModeResponse {
    mode: Mode,
}

#[derive(Serialize)]
struct AddProjectRequest {
    repo: String,
}

#[derive(Serialize)]
struct ChatRequest {
    message: String,
}

// =============================================================================
// Errors
// =============================================================================

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("HTTP status error: {0}")]
    HttpStatus(u16),
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_display() {
        assert_eq!(task_state_display_name(&TaskState::Running), "Running");
        assert_eq!(
            task_state_display_name(&TaskState::AwaitingMerge),
            "Changes Submitted"
        );
    }

    #[test]
    fn task_state_active_excludes_awaiting_merge() {
        assert!(task_state_is_active(&TaskState::Running));
        assert!(task_state_is_active(&TaskState::Question));
        assert!(task_state_is_active(&TaskState::Testing));
        assert!(
            !task_state_is_active(&TaskState::AwaitingMerge),
            "AwaitingMerge should NOT be active"
        );
        assert!(!task_state_is_active(&TaskState::Waiting));
        assert!(!task_state_is_active(&TaskState::Completed));
    }

    #[test]
    fn task_state_is_terminal() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(TaskState::Cancelled.is_terminal());
        assert!(!TaskState::Running.is_terminal());
    }

    #[test]
    fn task_source_label_display() {
        let issue = TaskSource::GithubIssue {
            owner: "foo".to_string(),
            repo: "bar".to_string(),
            number: 123,
        };
        assert_eq!(task_source_label(&issue), "foo/bar#123");

        let pr = TaskSource::GithubPr {
            owner: "foo".to_string(),
            repo: "bar".to_string(),
            number: 456,
        };
        assert_eq!(task_source_label(&pr), "foo/bar#456 (PR)");
    }
}
