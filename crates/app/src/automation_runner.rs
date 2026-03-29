//! Automation runner — executes automation runs inside container sessions.
//!
//! Instead of the direct LLM call used by `AutomationExecutor`, this module
//! starts a full container session (the same path tasks use) so the agent
//! has tool access, git, and a real working directory.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
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
/// (`TaskStateCompleted`, `TaskStateAwaitingMerge`, `TaskStateFailed`) or a
/// `SystemTimeLimitHard` event, this listener calls
/// `server.complete_automation_run()` or `server.fail_automation_run()`.
///
/// Both `TaskStateCompleted` and `TaskStateAwaitingMerge` are treated as successful
/// completion — the agent may create a PR and wait for merge, which is a valid
/// successful outcome for an automation run.
///
/// On broadcast lag, the listener recovers by scanning the store for any
/// automation runs still in `Running` state past their expected deadline and
/// forcibly failing them, preventing runs from getting permanently stuck.
///
/// The `shutdown_rx` receiver allows graceful shutdown of the listener.
pub fn spawn_automation_event_listener(
    event_bus: &EventBus,
    server: Arc<Server>,
    hard_limit: Duration,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> JoinHandle<()> {
    let mut rx = event_bus.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
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
                                // Hard time limit — session is being killed
                                events::EventType::SystemTimeLimitHard => {
                                    warn!(
                                        run_id = %run_id,
                                        "automation session hit hard time limit, marking run as failed"
                                    );
                                    if let Err(e) = server
                                        .fail_automation_run(
                                            &run_id,
                                            "session exceeded hard time limit".to_string(),
                                        )
                                        .await
                                    {
                                        error!(
                                            run_id = %run_id,
                                            error = %e,
                                            "failed to mark automation run as failed after hard time limit"
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
                                "automation event listener lagged — {n} events dropped, recovering"
                            );
                            // Recover: scan for running automation runs that are past
                            // their deadline and forcibly fail them.
                            recover_from_lag(&server, hard_limit).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!("event bus closed, automation event listener shutting down");
                            break;
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("automation event listener received shutdown signal");
                    break;
                }
            }
        }
    })
}

/// Recover from a broadcast lag by scanning for stuck automation runs.
///
/// Any run in `Running` state whose `started_at` is older than
/// `hard_limit + WATCHDOG_BUFFER` is forcibly failed. This ensures that even
/// if the terminal event was among the dropped messages, the run will not
/// stay stuck forever.
async fn recover_from_lag(server: &Server, hard_limit: Duration) {
    let runs = match server.list_running_automation_runs() {
        Ok(runs) => runs,
        Err(e) => {
            error!(error = %e, "failed to list running automation runs during lag recovery");
            return;
        }
    };

    let now = Utc::now();
    for run in runs {
        let elapsed = now.signed_duration_since(run.started_at);
        let deadline = hard_limit + WATCHDOG_BUFFER;
        if elapsed > chrono::Duration::from_std(deadline).unwrap_or(chrono::Duration::max_value()) {
            warn!(
                run_id = %run.id,
                elapsed_secs = elapsed.num_seconds(),
                "lag recovery: failing stuck automation run"
            );
            if let Err(e) = server
                .fail_automation_run(
                    &run.id,
                    "automation run stuck after broadcast lag — forcibly failed by recovery".to_string(),
                )
                .await
            {
                error!(run_id = %run.id, error = %e, "lag recovery: failed to mark run as failed");
            }
        }
    }
}

/// Buffer added to the hard limit before the watchdog considers a run stuck.
/// Gives time for normal shutdown and event propagation.
const WATCHDOG_BUFFER: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Spawn a periodic watchdog that scans for automation runs stuck in `Running`
/// state past their expected deadline (`hard_limit + buffer`).
///
/// This is a safety net that catches runs stuck for any reason — not just
/// broadcast lag. It runs every `interval` and forcibly fails any overdue runs.
pub fn spawn_automation_watchdog(
    server: Arc<Server>,
    hard_limit: Duration,
    interval: Duration,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let runs = match server.list_running_automation_runs() {
                        Ok(runs) => runs,
                        Err(e) => {
                            error!(error = %e, "watchdog: failed to list running automation runs");
                            continue;
                        }
                    };

                    let now = Utc::now();
                    let deadline = hard_limit + WATCHDOG_BUFFER;
                    for run in runs {
                        let elapsed = now.signed_duration_since(run.started_at);
                        if elapsed > chrono::Duration::from_std(deadline).unwrap_or(chrono::Duration::max_value()) {
                            warn!(
                                run_id = %run.id,
                                elapsed_secs = elapsed.num_seconds(),
                                "watchdog: failing stuck automation run"
                            );
                            if let Err(e) = server
                                .fail_automation_run(
                                    &run.id,
                                    "automation run exceeded hard time limit — forcibly failed by watchdog".to_string(),
                                )
                                .await
                            {
                                error!(
                                    run_id = %run.id,
                                    error = %e,
                                    "watchdog: failed to mark run as failed"
                                );
                            }
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("automation watchdog received shutdown signal");
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
