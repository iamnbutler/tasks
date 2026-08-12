//! Request and response bodies that aren't themselves domain types.
//!
//! Each shape derives both `Serialize` and `Deserialize`: the server
//! deserializes requests and serializes responses, a client does the
//! reverse, and both use these definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{BriefingSection, Build, Mode, SpecId, TaskId};

/// Body of `POST /projects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateProject {
    pub repo_owner: String,
    pub repo_name: String,
}

/// Body of `POST /queue/reorder`: the complete queue order, front to back.
/// Tasks not listed are left unranked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReorderQueue {
    pub task_ids: Vec<TaskId>,
}

/// Body of `POST /spec-queue/reorder`. Same semantics as [`ReorderQueue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReorderSpecQueue {
    pub spec_ids: Vec<SpecId>,
}

/// Body of `POST /spec-queue/{spec_id}/review`. `status` is a string rather
/// than a [`crate::models::SpecQueueStatus`] so the server can answer an
/// unknown value with a 400 of its own instead of a deserialization
/// rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub status: String,
    #[serde(default)]
    pub feedback: Option<String>,
}

/// Body of `POST /builds`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildRequest {
    pub spec_ids: Vec<SpecId>,
    /// Branch the batch is cut from and PR'd against. Defaults to `main`.
    #[serde(default)]
    pub base_branch: Option<String>,
}

/// A build with its batch, in position order — the shape of
/// `GET /builds/{id}` and the `POST /builds` acknowledgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildDetail {
    #[serde(flatten)]
    pub build: Build,
    pub spec_ids: Vec<SpecId>,
}

/// Body of `POST /orchestrator/messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendMessage {
    pub content: String,
}

/// Body of `POST /mode`. `mode` is a string for the same reason as
/// [`ReviewRequest::status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetMode {
    pub mode: String,
}

/// Response of `GET /mode` and `POST /mode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeResponse {
    pub mode: Mode,
}

/// One Home briefing slot as `GET /briefings` serves it. All three sections
/// are always present; a never-generated one has no content and reads as
/// stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefingStatus {
    pub section: BriefingSection,
    pub content: Option<String>,
    pub generated_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub regenerating: bool,
    /// The last generation failure, if the most recent attempt failed. The
    /// stored content (if any) is still served alongside it — never a blank
    /// slot, never a fabricated one.
    pub error: Option<String>,
}

/// Error body every non-2xx response carries: `{"error": "..."}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
