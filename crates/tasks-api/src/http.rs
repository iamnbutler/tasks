//! Request and response bodies that aren't themselves domain types.
//!
//! Each shape derives both `Serialize` and `Deserialize`: the server
//! deserializes requests and serializes responses, a client does the
//! reverse, and both use these definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{BriefingSection, Build, Mode, ProjectId, SpecId, TaskId};

/// Body of `POST /projects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateProject {
    pub repo_owner: String,
    pub repo_name: String,
}

/// The answer to a write the charter shadowed: the decision was recorded and
/// nothing happened. Deliberately not the normal success body — a shadow run
/// whose responses look like real ones is an evaluation that lies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowAck {
    /// Always `true`; present so a caller can branch on one field.
    pub shadowed: bool,
    /// The ledger row holding the judgment that was not applied.
    pub decision_seq: i64,
    /// What did not happen, in words, for the agent reading the response.
    pub note: String,
}

/// Body of `POST /charter/{capability}` — set what the orchestrator may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetCharter {
    /// `off`, `shadow`, or `live`.
    pub level: String,
    /// Optional manual brake: actions per day. `null` (the default) is
    /// uncapped, which is what every capability ships as.
    #[serde(default)]
    pub daily_limit: Option<i64>,
}

/// Body of `POST /issues` — file an issue upstream and track it here.
///
/// The write happens on the server rather than through an agent's own `gh`
/// credential, which is what makes it show up in the ledger, in the event log,
/// and under whatever caps the charter sets. An agent filing issues out of band
/// is not a smaller version of this; it is the ungoverned version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureIssue {
    /// Which repository. Optional when exactly one project is configured —
    /// the common case, and one less thing for a caller to get wrong.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Where this came from — "discovered while reviewing spec_… for #812".
    /// Required of the orchestrator and rendered into the issue body, so the
    /// capture is auditable from GitHub alone and a human can judge whether
    /// the instinct is set too loose.
    #[serde(default)]
    pub provenance: Option<String>,
    /// Why this is worth filing. Required of the orchestrator.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /tasks/{task_id}/close` — retire work upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseTaskRequest {
    /// `completed` or `not_planned`. Not cosmetic: `completed` claims the work
    /// was done and wants evidence to match, `not_planned` is a recalibration.
    pub reason: String,
    /// Why. Required of the orchestrator — "no longer relevant" is the one
    /// custodial call with no cheap evidence standard, so the reasoning is the
    /// only thing a human can review it by.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What was checked: the merged PR, the commit that did it. Free-form
    /// JSON, stored verbatim on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
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
    /// Sent onward to a scout on `needs_revision` — what to change.
    #[serde(default)]
    pub feedback: Option<String>,
    /// Why this verdict was rendered. Distinct from `feedback`: that is
    /// addressed to the scout, this is the decision record. Required when
    /// the caller is the orchestrator — an autonomous verdict with no stated
    /// reason is not auditable, so the server refuses it.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What the decider checked, free-form JSON (staleness, overlaps,
    /// verification claims). Stored verbatim on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /builds`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildRequest {
    pub spec_ids: Vec<SpecId>,
    /// Branch the batch is cut from and PR'd against. Defaults to `main`.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Why this batch, now. Required of the orchestrator — batching and
    /// ordering are judgment calls, so the reasoning is the record.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What the decider checked, free-form JSON. Stored on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
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
