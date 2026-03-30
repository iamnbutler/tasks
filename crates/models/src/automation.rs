//! Automation model — automations that run recurring or event-triggered tasks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Automation state — whether the automation is currently active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationState {
    /// Automation is active and will trigger on schedule/events.
    #[default]
    Active,
    /// Automation is paused — will not trigger, but can be resumed.
    Paused,
    /// Automation is disabled — will not trigger until re-enabled.
    Disabled,
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
    /// Run was cancelled by user.
    Cancelled,
}

impl RunStatus {
    /// Whether this is a terminal state (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
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
    /// When the workflow was last compiled. Used to detect staleness:
    /// if `updated_at > compiled_at`, the compiled workflow is outdated.
    pub compiled_at: Option<DateTime<Utc>>,
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
            compiled_at: None,
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

    /// Set the compiled workflow and record the compilation timestamp.
    pub fn set_compiled_workflow(&mut self, workflow: String) {
        self.compiled_workflow = Some(workflow);
        self.compiled_at = Some(Utc::now());
    }

    /// Whether the compiled workflow is stale (definition updated after last compilation).
    /// Returns `true` if there is a compiled workflow whose `compiled_at` predates `updated_at`,
    /// or if `compiled_at` is missing despite a workflow being present.
    pub fn is_compiled_workflow_stale(&self) -> bool {
        match (&self.compiled_workflow, self.compiled_at) {
            (Some(_), Some(compiled)) => self.updated_at > compiled,
            (Some(_), None) => true, // compiled but no timestamp — assume stale
            _ => false,              // no workflow at all — nothing to be stale
        }
    }
}

/// Maximum size in bytes for automation run output/error stored in SQLite.
/// Output exceeding this limit is truncated with a marker indicating truncation.
const MAX_OUTPUT_BYTES: usize = 1_024 * 1_024; // 1 MB

/// Truncate a string to fit within `MAX_OUTPUT_BYTES`, appending a truncation notice.
fn truncate_output(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    // Find a valid UTF-8 boundary at or before the limit, leaving room for the notice.
    let notice = "\n\n[truncated — output exceeded 1 MB limit]";
    let budget = MAX_OUTPUT_BYTES - notice.len();
    let end = s.floor_char_boundary(budget);
    let mut truncated = s;
    truncated.truncate(end);
    truncated.push_str(notice);
    truncated
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
    /// Output is truncated to 1 MB to prevent database bloat.
    pub fn complete(&mut self, output: Option<String>) {
        self.status = RunStatus::Completed;
        self.completed_at = Some(Utc::now());
        self.output = output.map(truncate_output);
    }

    /// Mark the run as failed.
    /// Error text is truncated to 1 MB to prevent database bloat.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = RunStatus::Failed;
        self.completed_at = Some(Utc::now());
        self.error = Some(truncate_output(error.into()));
    }

    /// Mark the run as cancelled.
    pub fn cancel(&mut self) {
        self.status = RunStatus::Cancelled;
        self.completed_at = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_output_short_string_unchanged() {
        let s = "hello world".to_string();
        assert_eq!(truncate_output(s.clone()), s);
    }

    #[test]
    fn truncate_output_at_exact_limit() {
        let s = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate_output(s.clone()), s);
    }

    #[test]
    fn truncate_output_over_limit() {
        let s = "a".repeat(MAX_OUTPUT_BYTES + 1000);
        let result = truncate_output(s);
        assert!(result.len() <= MAX_OUTPUT_BYTES);
        assert!(result.ends_with("[truncated — output exceeded 1 MB limit]"));
    }

    #[test]
    fn truncate_output_respects_utf8_boundaries() {
        // Build a string with multi-byte chars that goes over the limit
        let base = "é".repeat(MAX_OUTPUT_BYTES); // each é is 2 bytes
        assert!(base.len() > MAX_OUTPUT_BYTES);
        let result = truncate_output(base);
        assert!(result.len() <= MAX_OUTPUT_BYTES);
        // Must be valid UTF-8
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn automation_run_complete_truncates() {
        let mut run = AutomationRun::new("r1", "a1");
        let big_output = "x".repeat(MAX_OUTPUT_BYTES + 5000);
        run.complete(Some(big_output));
        let output = run.output.unwrap();
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert!(output.ends_with("[truncated — output exceeded 1 MB limit]"));
    }

    #[test]
    fn automation_run_fail_truncates() {
        let mut run = AutomationRun::new("r1", "a1");
        let big_error = "e".repeat(MAX_OUTPUT_BYTES + 5000);
        run.fail(big_error);
        let error = run.error.unwrap();
        assert!(error.len() <= MAX_OUTPUT_BYTES);
        assert!(error.ends_with("[truncated — output exceeded 1 MB limit]"));
    }

    #[test]
    fn automation_run_complete_none_output() {
        let mut run = AutomationRun::new("r1", "a1");
        run.complete(None);
        assert!(run.output.is_none());
    }
}
