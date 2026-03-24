//! Automation model — automations that run recurring or event-triggered tasks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Automation state — whether the automation is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationState {
    /// Automation is active and will trigger on schedule/events.
    Active,
    /// Automation is paused — will not trigger, but can be resumed.
    Paused,
    /// Automation is disabled — will not trigger until re-enabled.
    Disabled,
}

impl Default for AutomationState {
    fn default() -> Self {
        Self::Active
    }
}

/// Trigger type — what causes the automation to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerType {
    /// Scheduled trigger using cron expression.
    Schedule { cron: String },
    /// Event-driven trigger (e.g., "issue:created", "pr:merged").
    Event { event_type: String },
    /// Manual trigger only — no automatic execution.
    Manual,
}

/// Run status — the state of an automation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run is queued, waiting to start.
    Pending,
    /// Run is currently executing.
    Running,
    /// Run completed successfully.
    Completed,
    /// Run failed with an error.
    Failed,
}

impl RunStatus {
    /// Whether this is a terminal state (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// An automation — a reusable workflow that can be triggered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Automation {
    /// Unique automation ID.
    pub id: String,
    /// Project this automation belongs to.
    pub project_id: String,
    /// Human-readable name.
    pub name: String,
    /// Natural language prompt describing what the automation should do.
    pub prompt: String,
    /// Compiled/hardened workflow (can be deferred until first run).
    pub compiled_workflow: Option<String>,
    /// What triggers this automation.
    pub trigger: TriggerType,
    /// Current state of the automation.
    pub state: AutomationState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Automation {
    /// Create a new automation in the Active state.
    pub fn new(
        id: impl Into<String>,
        project_id: impl Into<String>,
        name: impl Into<String>,
        prompt: impl Into<String>,
        trigger: TriggerType,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            project_id: project_id.into(),
            name: name.into(),
            prompt: prompt.into(),
            compiled_workflow: None,
            trigger,
            state: AutomationState::Active,
            created_at: now,
            updated_at: now,
        }
    }

    /// Set the automation state, updating the timestamp.
    pub fn set_state(&mut self, state: AutomationState) {
        self.state = state;
        self.updated_at = Utc::now();
    }
}

/// A single run of an automation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationRun {
    /// Unique run ID.
    pub id: String,
    /// The automation that was run.
    pub automation_id: String,
    /// Current status of the run.
    pub status: RunStatus,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run completed (if terminal).
    pub completed_at: Option<DateTime<Utc>>,
    /// Output from a successful run.
    pub output: Option<String>,
    /// Error message from a failed run.
    pub error: Option<String>,
}

impl AutomationRun {
    /// Create a new run in the Pending state.
    pub fn new(id: impl Into<String>, automation_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            automation_id: automation_id.into(),
            status: RunStatus::Pending,
            started_at: Utc::now(),
            completed_at: None,
            output: None,
            error: None,
        }
    }

    /// Mark the run as running.
    pub fn start(&mut self) {
        self.status = RunStatus::Running;
        self.started_at = Utc::now();
    }

    /// Mark the run as completed successfully.
    pub fn complete(&mut self, output: Option<String>) {
        self.status = RunStatus::Completed;
        self.completed_at = Some(Utc::now());
        self.output = output;
    }

    /// Mark the run as failed.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = RunStatus::Failed;
        self.completed_at = Some(Utc::now());
        self.error = Some(error.into());
    }
}
