//! Automation runner — executes automation runs via agent sessions.
//!
//! This module bridges the gap between automation run creation and actual execution
//! by starting sessions and monitoring their completion.

use std::sync::Arc;

use tracing::{error, info, warn};
use uuid::Uuid;

use events::{Actor, Event, EventType};
use models::automation::{AutomationRun, RunStatus};
use runtime::ContainerRuntime;
use server::automation_executor::{
    build_automation_prompt, run_session_id, session_to_run_id, AutomationContext,
};
use server::Server;
use tasks_session::SessionManager;

/// Error type for automation runner operations.
#[derive(Debug)]
pub enum AutomationRunnerError {
    AutomationNotFound(String),
    ProjectNotFound(String),
    SessionError(String),
    ServerError(server::ServerError),
}

impl std::fmt::Display for AutomationRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AutomationNotFound(id) => write!(f, "automation not found: {}", id),
            Self::ProjectNotFound(id) => write!(f, "project not found: {}", id),
            Self::SessionError(msg) => write!(f, "session error: {}", msg),
            Self::ServerError(e) => write!(f, "server error: {}", e),
        }
    }
}

impl std::error::Error for AutomationRunnerError {}

impl From<server::ServerError> for AutomationRunnerError {
    fn from(e: server::ServerError) -> Self {
        Self::ServerError(e)
    }
}

/// Execute an automation run by starting an agent session.
///
/// This function:
/// 1. Gets the automation and project information
/// 2. Builds the automation prompt
/// 3. Starts a session with the session manager
/// 4. Returns the run (monitoring handles completion asynchronously)
///
/// The session manager will emit events that should be handled by the
/// automation run event handler to update run status.
pub async fn execute_automation_run<R: ContainerRuntime + Clone + Send + Sync + 'static>(
    server: &Arc<Server>,
    session_manager: &Arc<SessionManager<R>>,
    automation_id: &str,
) -> Result<AutomationRun, AutomationRunnerError> {
    // Get the automation
    let automation = server
        .get_automation(automation_id)
        .await
        .ok_or_else(|| AutomationRunnerError::AutomationNotFound(automation_id.to_string()))?;

    // Get the project
    let project = server
        .get_project(&automation.project_id)
        .await
        .ok_or_else(|| AutomationRunnerError::ProjectNotFound(automation.project_id.clone()))?;

    // Get previous run output for context (if any)
    let previous_output = get_previous_run_output(server, automation_id);

    // Build the automation context
    let ctx = AutomationContext {
        automation: automation.clone(),
        project: project.clone(),
        previous_output,
    };

    // Build the prompt
    let prompt = build_automation_prompt(&ctx);

    // Create the run record
    let run = server.create_automation_run(automation_id).await?;
    let session_id = run_session_id(&run.id);

    // Build repo URL
    let repo_url = format!("https://github.com/{}.git", project.repo);

    // For automations, we use a temporary branch with a unique suffix.
    // Unlike tasks, automations don't create PRs, so the branch is just for isolation.
    let unique_suffix = &Uuid::new_v4().to_string()[..8];
    let branch = format!("automation/{}--{}", automation.id, unique_suffix);

    info!(
        automation_id = %automation_id,
        run_id = %run.id,
        session_id = %session_id,
        "starting automation run session"
    );

    // Start the session
    if let Err(e) = session_manager
        .start_session(session_id.clone(), repo_url, branch, prompt, None, None)
        .await
    {
        // If session fails to start, mark the run as failed
        error!(
            run_id = %run.id,
            error = %e,
            "failed to start automation session"
        );

        // Update run status to failed
        if let Err(e2) = update_run_failed(server, &run.id, &format!("Session start failed: {}", e))
        {
            error!(run_id = %run.id, error = %e2, "failed to update run status");
        }

        return Err(AutomationRunnerError::SessionError(e.to_string()));
    }

    info!(
        run_id = %run.id,
        session_id = %session_id,
        "automation session started"
    );

    Ok(run)
}

/// Get the output from the most recent completed run of an automation.
fn get_previous_run_output(server: &Server, automation_id: &str) -> Option<String> {
    if let Ok(runs) = server.list_automation_runs(automation_id) {
        // Runs are ordered by started_at DESC, so the first completed one is the most recent
        for run in runs {
            if run.status == RunStatus::Completed {
                return run.output;
            }
        }
    }
    None
}

/// Update a run's status to failed.
fn update_run_failed(server: &Server, run_id: &str, error: &str) -> Result<(), server::ServerError> {
    // For now, we'll emit an event and let the event handler update the store.
    // This is because we don't have direct access to modify the run in the server.
    // The proper solution would be to add a method to Server to update run status.

    // The event handler will pick this up and update the run
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let event = Event::new(
                EventType::AutomationRunFailed,
                run_id,
                Actor::System,
                serde_json::json!({
                    "error": error,
                }),
            );
            server.event_bus.publish(event).await
        })
    })?;
    Ok(())
}

/// Handle events for automation run sessions.
///
/// This function should be called from the main event loop to process
/// events related to automation runs. It maps task state events to
/// automation run events when the task_id is an automation session ID.
pub async fn handle_automation_event(server: &Arc<Server>, event: &Event) {
    // Check if this is an automation run session (task field contains session ID)
    let Some(run_id) = session_to_run_id(&event.task) else {
        return;
    };

    match &event.event_type {
        // Agent messages - accumulate output
        EventType::AgentMessage => {
            if let Some(text) = event.data.get("text").and_then(|v| v.as_str()) {
                // Append to run output
                append_run_output(server, run_id, text).await;
            }
        }

        // Session completed successfully
        EventType::TaskStateCompleted => {
            info!(run_id = %run_id, "automation run completed");
            complete_run(server, run_id).await;
        }

        // Session failed
        EventType::TaskStateFailed => {
            let error = event
                .data
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            warn!(run_id = %run_id, error = %error, "automation run failed");
            fail_run(server, run_id, error).await;
        }

        // Session was cancelled/waiting - treat as failure for automations
        EventType::TaskStateCancelled | EventType::TaskStateWaiting => {
            let reason = event
                .data
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("session ended unexpectedly");
            warn!(run_id = %run_id, reason = %reason, "automation run ended unexpectedly");
            fail_run(server, run_id, reason).await;
        }

        _ => {}
    }
}

/// Append output to a run (in-memory accumulation).
///
/// For now, we store output in a static map. In a production system,
/// this should be persisted incrementally.
async fn append_run_output(server: &Arc<Server>, run_id: &str, text: &str) {
    // Get the current run and update it with accumulated output
    // For simplicity, we'll update the store when the run completes
    // This uses a thread-local accumulator

    // Emit an event for real-time output streaming
    let event = Event::new(
        EventType::AgentMessage,
        &format!("automation-run:{}", run_id),
        Actor::Agent,
        serde_json::json!({
            "text": text,
            "run_id": run_id,
        }),
    );
    if let Err(e) = server.event_bus.publish(event).await {
        error!(run_id = %run_id, error = %e, "failed to publish agent message event");
    }
}

/// Mark a run as completed.
async fn complete_run(server: &Arc<Server>, run_id: &str) {
    // Update the run in the store
    if let Err(e) = server.complete_automation_run(run_id, None).await {
        error!(run_id = %run_id, error = %e, "failed to complete automation run");
        return;
    }

    // Emit completion event
    let event = Event::new(
        EventType::AutomationRunCompleted,
        run_id,
        Actor::System,
        serde_json::json!({}),
    );
    if let Err(e) = server.event_bus.publish(event).await {
        error!(run_id = %run_id, error = %e, "failed to publish run completed event");
    }
}

/// Mark a run as failed.
async fn fail_run(server: &Arc<Server>, run_id: &str, error: &str) {
    // Update the run in the store
    if let Err(e) = server.fail_automation_run(run_id, error).await {
        error!(run_id = %run_id, error = %e, "failed to fail automation run");
        return;
    }

    // Emit failure event
    let event = Event::new(
        EventType::AutomationRunFailed,
        run_id,
        Actor::System,
        serde_json::json!({ "error": error }),
    );
    if let Err(e) = server.event_bus.publish(event).await {
        error!(run_id = %run_id, error = %e, "failed to publish run failed event");
    }
}
