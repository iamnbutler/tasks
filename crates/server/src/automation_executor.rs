//! Automation executor — runs automations via agent sessions.
//!
//! When an automation run is triggered (manual or scheduled), this module:
//! 1. Creates a session for the automation
//! 2. Passes the automation prompt to the agent
//! 3. Monitors the session and updates the run status
//! 4. Captures output when the agent finishes

use std::collections::HashMap;

use models::automation::Automation;
use models::project::Project;

/// Context provided to the automation agent.
#[derive(Debug, Clone)]
pub struct AutomationContext {
    /// The automation being executed.
    pub automation: Automation,
    /// The project this automation belongs to.
    pub project: Project,
    /// Previous run output (for trend analysis).
    pub previous_output: Option<String>,
}

/// Build the prompt for an automation run.
///
/// Constructs a prompt that includes:
/// - The automation's natural language prompt
/// - Project context (repo URL, name)
/// - Previous run output if available
pub fn build_automation_prompt(ctx: &AutomationContext) -> String {
    let mut prompt = String::new();

    // Header with automation context
    prompt.push_str("# Automation Run\n\n");
    prompt.push_str(&format!("**Automation:** {}\n", ctx.automation.name));
    prompt.push_str(&format!("**Project:** {}\n", ctx.project.repo));
    prompt.push_str("\n---\n\n");

    // The main automation prompt
    prompt.push_str("## Task\n\n");
    prompt.push_str(&ctx.automation.prompt);
    prompt.push_str("\n\n");

    // Previous run context if available
    if let Some(prev) = &ctx.previous_output {
        prompt.push_str("## Previous Run Output\n\n");
        prompt.push_str("For context, here is the output from the previous run:\n\n");
        prompt.push_str("```\n");
        prompt.push_str(prev);
        prompt.push_str("\n```\n\n");
    }

    // Instructions for automation behavior
    prompt.push_str("## Instructions\n\n");
    prompt.push_str("This is an automation run, not a regular task. Key differences:\n");
    prompt.push_str("- Focus on the specific automation task described above\n");
    prompt.push_str("- Report findings and take actions as specified\n");
    prompt.push_str("- Provide a clear summary of what was done and any issues found\n");

    prompt
}

/// Mapping of automation run IDs to their session identifiers.
///
/// The session manager uses task IDs to identify sessions. For automation runs,
/// we use a prefixed run ID to avoid collisions with task IDs.
pub fn run_session_id(run_id: &str) -> String {
    format!("automation-run:{}", run_id)
}

/// Extract the run ID from a session ID, if it's an automation run session.
pub fn session_to_run_id(session_id: &str) -> Option<&str> {
    session_id.strip_prefix("automation-run:")
}

/// Track active automation runs and their accumulated output.
#[derive(Debug, Default)]
pub struct AutomationRunTracker {
    /// Map from run ID to accumulated output lines.
    outputs: HashMap<String, Vec<String>>,
}

impl AutomationRunTracker {
    pub fn new() -> Self {
        Self {
            outputs: HashMap::new(),
        }
    }

    /// Start tracking a new run.
    pub fn start_run(&mut self, run_id: &str) {
        self.outputs.insert(run_id.to_string(), Vec::new());
    }

    /// Append output to a run.
    pub fn append_output(&mut self, run_id: &str, line: &str) {
        if let Some(output) = self.outputs.get_mut(run_id) {
            output.push(line.to_string());
        }
    }

    /// Get and remove the accumulated output for a completed run.
    pub fn take_output(&mut self, run_id: &str) -> Option<String> {
        self.outputs.remove(run_id).map(|lines| lines.join("\n"))
    }

    /// Check if a run is being tracked.
    pub fn is_tracking(&self, run_id: &str) -> bool {
        self.outputs.contains_key(run_id)
    }

    /// Remove a run from tracking (for cleanup on failure).
    pub fn remove(&mut self, run_id: &str) {
        self.outputs.remove(run_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use models::automation::TriggerType;

    fn make_automation() -> Automation {
        Automation::new(
            "auto-1",
            "proj-1",
            "Check Documentation",
            "Review the documentation and report any outdated sections.",
            TriggerType::Manual,
        )
    }

    fn make_project() -> Project {
        Project::new("proj-1", "acme/widgets")
    }

    #[test]
    fn test_build_automation_prompt() {
        let ctx = AutomationContext {
            automation: make_automation(),
            project: make_project(),
            previous_output: None,
        };

        let prompt = build_automation_prompt(&ctx);

        assert!(prompt.contains("Check Documentation"));
        assert!(prompt.contains("acme/widgets"));
        assert!(prompt.contains("Review the documentation"));
        assert!(!prompt.contains("Previous Run Output"));
    }

    #[test]
    fn test_build_automation_prompt_with_previous() {
        let ctx = AutomationContext {
            automation: make_automation(),
            project: make_project(),
            previous_output: Some("Found 3 outdated sections".to_string()),
        };

        let prompt = build_automation_prompt(&ctx);

        assert!(prompt.contains("Previous Run Output"));
        assert!(prompt.contains("Found 3 outdated sections"));
    }

    #[test]
    fn test_run_session_id() {
        assert_eq!(run_session_id("run-123"), "automation-run:run-123");
    }

    #[test]
    fn test_session_to_run_id() {
        assert_eq!(session_to_run_id("automation-run:run-123"), Some("run-123"));
        assert_eq!(session_to_run_id("task-456"), None);
    }

    #[test]
    fn test_run_tracker() {
        let mut tracker = AutomationRunTracker::new();

        tracker.start_run("run-1");
        assert!(tracker.is_tracking("run-1"));

        tracker.append_output("run-1", "Line 1");
        tracker.append_output("run-1", "Line 2");

        let output = tracker.take_output("run-1").unwrap();
        assert_eq!(output, "Line 1\nLine 2");
        assert!(!tracker.is_tracking("run-1"));
    }
}
