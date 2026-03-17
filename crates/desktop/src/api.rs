//! HTTP API client for the Tasks server.
//!
//! This module provides type-safe access to the Tasks REST API endpoints.
//! It mirrors the TypeScript API client in `web/src/lib/api.ts`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default server URL.
pub const DEFAULT_SERVER_URL: &str = "http://localhost:4800";

/// API client for the Tasks server.
#[derive(Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    base_url: String,
}

impl ApiClient {
    /// Create a new API client with the given base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
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

// --- API Types ---
// These mirror the TypeScript types in web/src/lib/types.ts

/// Operating mode (spec Section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Stop,
    Pause,
    Play,
}

impl Mode {
    pub fn display_name(&self) -> &'static str {
        match self {
            Mode::Stop => "Stop",
            Mode::Pause => "Pause",
            Mode::Play => "Play",
        }
    }
}

/// Task state (spec Section 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Waiting,
    Blocked,
    Running,
    Question,
    Testing,
    AwaitingMerge,
    Conflict,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn display_name(&self) -> &'static str {
        match self {
            TaskState::Waiting => "Waiting",
            TaskState::Blocked => "Blocked",
            TaskState::Running => "Running",
            TaskState::Question => "Question",
            TaskState::Testing => "Testing",
            TaskState::AwaitingMerge => "Awaiting Merge",
            TaskState::Conflict => "Conflict",
            TaskState::Completed => "Completed",
            TaskState::Failed => "Failed",
            TaskState::Cancelled => "Cancelled",
        }
    }

    /// Whether this is a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Whether this is an active state (agent working).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Running | Self::Question | Self::Testing | Self::AwaitingMerge
        )
    }
}

/// Task source — origin reference (spec Section 5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskSource {
    GithubIssue {
        owner: String,
        repo: String,
        number: u64,
    },
    GithubPr {
        owner: String,
        repo: String,
        number: u64,
    },
    Internal,
}

impl TaskSource {
    /// Get a display-friendly label for the source.
    pub fn label(&self) -> String {
        match self {
            TaskSource::GithubIssue { owner, repo, number } => {
                format!("{}/{}#{}", owner, repo, number)
            }
            TaskSource::GithubPr { owner, repo, number } => {
                format!("{}/{}#{} (PR)", owner, repo, number)
            }
            TaskSource::Internal => "Internal".to_string(),
        }
    }
}

/// A task — the internal representation of a unit of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub source: TaskSource,
    pub title: String,
    pub description: Option<String>,
    pub state: TaskState,
    pub parent_id: Option<String>,
    pub blocked_by: Vec<String>,
    pub project: String,
    pub labels: Vec<String>,
    pub priority: Option<i32>,
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
    pub retry_count: u32,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A project — maps to a single repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub repo: String,
    pub default_branch: String,
    pub config: serde_json::Value,
}

/// Merge queue entry status (spec Section 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    Pending,
    Approved,
    Rejected,
    Merged,
    Conflict,
}

impl MergeStatus {
    pub fn display_name(&self) -> &'static str {
        match self {
            MergeStatus::Pending => "Pending",
            MergeStatus::Approved => "Approved",
            MergeStatus::Rejected => "Rejected",
            MergeStatus::Merged => "Merged",
            MergeStatus::Conflict => "Conflict",
        }
    }
}

/// A merge queue entry — a PR waiting to be merged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeQueueEntry {
    pub id: String,
    pub task_id: String,
    pub pr_url: String,
    pub status: MergeStatus,
    pub queued_at: DateTime<Utc>,
}

/// Who produced an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Human,
    Orchestrator,
    Scheduler,
    Agent,
    System,
}

impl Actor {
    pub fn display_name(&self) -> &'static str {
        match self {
            Actor::Human => "Human",
            Actor::Orchestrator => "Orchestrator",
            Actor::Scheduler => "Scheduler",
            Actor::Agent => "Agent",
            Actor::System => "System",
        }
    }
}

/// An event in the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub task: String,
    pub actor: Actor,
    pub ts: DateTime<Utc>,
    pub data: serde_json::Value,
}

/// Slot utilization info.
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

// --- Request/Response types ---

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

// --- Errors ---

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("HTTP status error: {0}")]
    HttpStatus(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_display() {
        assert_eq!(TaskState::Running.display_name(), "Running");
        assert_eq!(TaskState::AwaitingMerge.display_name(), "Awaiting Merge");
    }

    #[test]
    fn task_state_is_active() {
        assert!(TaskState::Running.is_active());
        assert!(TaskState::Question.is_active());
        assert!(!TaskState::Waiting.is_active());
        assert!(!TaskState::Completed.is_active());
    }

    #[test]
    fn task_state_is_terminal() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(TaskState::Cancelled.is_terminal());
        assert!(!TaskState::Running.is_terminal());
    }

    #[test]
    fn task_source_label() {
        let issue = TaskSource::GithubIssue {
            owner: "foo".to_string(),
            repo: "bar".to_string(),
            number: 123,
        };
        assert_eq!(issue.label(), "foo/bar#123");

        let pr = TaskSource::GithubPr {
            owner: "foo".to_string(),
            repo: "bar".to_string(),
            number: 456,
        };
        assert_eq!(pr.label(), "foo/bar#456 (PR)");
    }
}
