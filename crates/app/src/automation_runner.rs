//! Automation runner — executes automation runs inside container sessions.
//!
//! Instead of the direct LLM call used by `AutomationExecutor`, this module
//! starts a full container session (the same path tasks use) so the agent
//! has tool access, git, and a real working directory.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{error, info};
use uuid::Uuid;

use events::EventBus;
use runtime::AppleContainerRuntime;
use server::Server;
use tasks_session::{SessionLimits, SessionManager};

/// Prefix used for automation session IDs.
const SESSION_PREFIX: &str = "automation-run:";

/// Execute an automation run inside a container session.
///
/// Creates a session with the ID `automation-run:{run_id}`. If the session
/// fails to start, the run is marked as failed via `server.fail_automation_run()`.
///
/// The `soft_limit` and `hard_limit` parameters allow customization of the
/// session time limits. Automation sessions typically use shorter limits
/// (25m soft, 30m hard) than regular task sessions.
pub async fn execute_automation_run(
    session_manager: &SessionManager<AppleContainerRuntime>,
    server: &Arc<Server>,
    run_id: &str,
    automation_id: &str,
    soft_limit: Duration,
    hard_limit: Duration,
) {
    // Look up the automation to get its prompt and project.
    let (prompt, project_id) = {
        let state = server.state.read().await;
        match state.automations.get(automation_id) {
            Some(automation) => (automation.prompt.clone(), automation.project_id.clone()),
            None => {
                error!(
                    run_id = %run_id,
                    automation_id = %automation_id,
                    "automation not found"
                );
                if let Err(e) = server
                    .fail_automation_run(run_id, format!("automation not found: {automation_id}"))
                    .await
                {
                    error!(run_id = %run_id, error = %e, "failed to mark automation run as failed");
                }
                return;
            }
        }
    };

    // Resolve the project to get the repo URL.
    let repo_url = match server.get_project(&project_id).await {
        Some(p) => format!("https://github.com/{}.git", p.repo),
        None => {
            error!(run_id = %run_id, project_id = %project_id, "project not found");
            if let Err(e) = server
                .fail_automation_run(run_id, format!("project not found: {project_id}"))
                .await
            {
                error!(run_id = %run_id, error = %e, "failed to mark automation run as failed");
            }
            return;
        }
    };

    // Automation sessions work on a unique branch so they don't collide.
    let unique_suffix = &Uuid::new_v4().to_string()[..8];
    let branch = format!("automations/{}--{}", run_id, unique_suffix);

    let session_id = format!("{SESSION_PREFIX}{run_id}");

    // Prepend time limit notice to the prompt so agent can plan accordingly
    let hard_limit_mins = hard_limit.as_secs() / 60;
    let prompt_with_notice = format!(
        "[TIME LIMIT] This automation session has a {hard_limit_mins}-minute hard limit. \
        Plan your work to complete within this time. If you cannot finish, prioritize \
        the most important changes and commit partial progress.\n\n{prompt}"
    );

    info!(
        run_id = %run_id,
        automation_id = %automation_id,
        session_id = %session_id,
        soft_limit_secs = soft_limit.as_secs(),
        hard_limit_secs = hard_limit.as_secs(),
        "starting automation container session"
    );

    let time_limits = SessionLimits {
        soft_limit: Some(soft_limit),
        hard_limit: Some(hard_limit),
    };

    if let Err(e) = session_manager
        .start_session_with_limits(
            session_id,
            repo_url,
            branch,
            prompt_with_notice,
            None,
            None,
            time_limits,
        )
        .await
    {
        error!(
            run_id = %run_id,
            error = %e,
            "failed to start automation session"
        );
        if let Err(e2) = server
            .fail_automation_run(run_id, format!("failed to start session: {e}"))
            .await
        {
            error!(run_id = %run_id, error = %e2, "failed to mark automation run as failed");
        }
    }
}

/// Returns `true` if the session ID belongs to an automation run.
pub fn is_automation_session(session_id: &str) -> bool {
    session_id.starts_with(SESSION_PREFIX)
}

/// Extract the run ID from an automation session ID.
///
/// Returns `None` if the session ID does not have the automation prefix.
pub fn run_id_from_session(session_id: &str) -> Option<&str> {
    session_id.strip_prefix(SESSION_PREFIX)
}

/// Spawn a background task that listens for session completion/failure events
/// and updates automation run records accordingly.
///
/// When a session with an `automation-run:` prefix reaches a terminal state
/// (`TaskStateCompleted`, `TaskStateAwaitingMerge`, or `TaskStateFailed`), this
/// listener calls `server.complete_automation_run()` or `server.fail_automation_run()`.
///
/// Both `TaskStateCompleted` and `TaskStateAwaitingMerge` are treated as successful
/// completion — the agent may create a PR and wait for merge, which is a valid
/// successful outcome for an automation run.
pub fn spawn_automation_event_listener(
    event_bus: &EventBus,
    server: Arc<Server>,
) -> JoinHandle<()> {
    let mut rx = event_bus.subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Only care about automation sessions
                    let run_id = match run_id_from_session(&event.task) {
                        Some(id) => id.to_string(),
                        None => continue,
                    };

                    match event.event_type {
                        // Agent exited successfully (with or without a PR)
                        events::EventType::TaskStateCompleted
                        | events::EventType::TaskStateAwaitingMerge => {
                            info!(
                                run_id = %run_id,
                                event_type = %event.event_type.as_str(),
                                "automation session completed, marking run as complete"
                            );
                            if let Err(e) = server.complete_automation_run(&run_id, None).await {
                                error!(
                                    run_id = %run_id,
                                    error = %e,
                                    "failed to complete automation run"
                                );
                            }
                        }
                        // Agent exited with failure
                        events::EventType::TaskStateFailed => {
                            let reason = event
                                .data
                                .get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("session failed");
                            info!(
                                run_id = %run_id,
                                reason = %reason,
                                "automation session failed, marking run as failed"
                            );
                            if let Err(e) = server
                                .fail_automation_run(&run_id, reason.to_string())
                                .await
                            {
                                error!(
                                    run_id = %run_id,
                                    error = %e,
                                    "failed to mark automation run as failed"
                                );
                            }
                        }
                        // Ignore all other event types
                        _ => {}
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    error!(
                        skipped = n,
                        "automation event listener lagged — {n} events dropped from broadcast channel"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("event bus closed, automation event listener shutting down");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_automation_session() {
        assert!(is_automation_session("automation-run:abc-123"));
        assert!(!is_automation_session("task-abc-123"));
        assert!(!is_automation_session(""));
    }

    #[test]
    fn test_run_id_from_session() {
        assert_eq!(
            run_id_from_session("automation-run:abc-123"),
            Some("abc-123")
        );
        assert_eq!(run_id_from_session("task-abc-123"), None);
        assert_eq!(run_id_from_session("automation-run:"), Some(""));
    }
}
