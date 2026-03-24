//! Automation runner — executes automation runs inside container sessions.
//!
//! Instead of the direct LLM call used by `AutomationExecutor`, this module
//! starts a full container session (the same path tasks use) so the agent
//! has tool access, git, and a real working directory.

use std::sync::Arc;

use tracing::{error, info};
use uuid::Uuid;

use runtime::AppleContainerRuntime;
use server::Server;
use tasks_session::SessionManager;

/// Prefix used for automation session IDs.
const SESSION_PREFIX: &str = "automation-run:";

/// Execute an automation run inside a container session.
///
/// Creates a session with the ID `automation-run:{run_id}`. If the session
/// fails to start, the run is marked as failed via `server.fail_automation_run()`.
pub async fn execute_automation_run(
    session_manager: &SessionManager<AppleContainerRuntime>,
    server: &Arc<Server>,
    run_id: &str,
    automation_id: &str,
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

    // Resolve the project to get the repo URL and default branch.
    let project = server.get_project(&project_id).await;
    let repo_url = project
        .as_ref()
        .map(|p| format!("https://github.com/{}.git", p.repo))
        .unwrap_or_default();

    // Automation sessions work on a unique branch so they don't collide.
    let unique_suffix = &Uuid::new_v4().to_string()[..8];
    let branch = format!("automations/{}--{}", run_id, unique_suffix);

    let session_id = format!("{SESSION_PREFIX}{run_id}");

    info!(
        run_id = %run_id,
        automation_id = %automation_id,
        session_id = %session_id,
        "starting automation container session"
    );

    if let Err(e) = session_manager
        .start_session(session_id, repo_url, branch, prompt, None, None)
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
