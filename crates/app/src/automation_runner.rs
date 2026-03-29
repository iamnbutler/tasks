//! Automation runner — executes automation runs inside container sessions.
//!
//! Instead of the direct LLM call used by `AutomationExecutor`, this module
//! starts a full container session (the same path tasks use) so the agent
//! has tool access, git, and a real working directory.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use uuid::Uuid;

use events::EventBus;
use runtime::AppleContainerRuntime;
use server::Server;
use tasks_session::{SessionLimits, SessionManager};

/// Prefix used for automation session IDs.
const SESSION_PREFIX: &str = "automation-run:";

/// Buffer added to hard_limit when detecting stuck runs.
/// Gives extra grace period before the watchdog intervenes.
const STUCK_RUN_BUFFER: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// How often the stuck-run watchdog checks for stuck runs.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

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

/// Handle a single event for an automation run, returning whether it was a
/// terminal event that was processed.
async fn handle_automation_event(
    event: &events::Event,
    server: &Server,
) -> bool {
    let run_id = match run_id_from_session(&event.task) {
        Some(id) => id.to_string(),
        None => return false,
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
            true
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
            true
        }
        // Hard time limit hit — the session will be killed, but if we miss
        // the subsequent TaskStateFailed event, this ensures we still fail
        // the run. The server methods are idempotent on terminal states.
        events::EventType::SystemTimeLimitHard => {
            warn!(
                run_id = %run_id,
                "automation session hit hard time limit, marking run as failed"
            );
            if let Err(e) = server
                .fail_automation_run(&run_id, "session exceeded hard time limit".to_string())
                .await
            {
                error!(
                    run_id = %run_id,
                    error = %e,
                    "failed to mark automation run as failed after hard time limit"
                );
            }
            true
        }
        _ => false,
    }
}

/// On broadcast lag, replay events from the event store to recover any missed
/// terminal events for automation runs that are still in Running state.
async fn recover_from_lag(event_bus: &EventBus, server: &Server) {
    // List all automation session IDs from the event store
    let task_ids = match event_bus.list_tasks().await {
        Ok(ids) => ids,
        Err(e) => {
            error!(error = %e, "failed to list tasks during lag recovery");
            return;
        }
    };

    for task_id in task_ids {
        let run_id = match run_id_from_session(&task_id) {
            Some(id) => id.to_string(),
            None => continue,
        };

        // Check if this run is still in a non-terminal state
        let run = match server.get_automation_run(&run_id) {
            Ok(Some(run)) if !run.status.is_terminal() => run,
            _ => continue,
        };

        // Replay events from store for this session
        let events = match event_bus.read_task(&task_id).await {
            Ok(events) => events,
            Err(e) => {
                error!(
                    run_id = %run_id,
                    error = %e,
                    "failed to read events during lag recovery"
                );
                continue;
            }
        };

        // Process events in order; stop at the first terminal one
        for event in &events {
            if handle_automation_event(event, server).await {
                info!(
                    run_id = %run.id,
                    event_type = %event.event_type.as_str(),
                    "recovered missed terminal event for automation run"
                );
                break;
            }
        }
    }
}

/// Spawn a background task that listens for session completion/failure events
/// and updates automation run records accordingly.
///
/// When a session with an `automation-run:` prefix reaches a terminal state
/// (`TaskStateCompleted`, `TaskStateAwaitingMerge`, or `TaskStateFailed`), this
/// listener calls `server.complete_automation_run()` or `server.fail_automation_run()`.
///
/// Also handles `SystemTimeLimitHard` as a redundant failure signal, so that
/// even if the subsequent `TaskStateFailed` event is missed, the run is failed.
///
/// Both `TaskStateCompleted` and `TaskStateAwaitingMerge` are treated as successful
/// completion — the agent may create a PR and wait for merge, which is a valid
/// successful outcome for an automation run.
///
/// On broadcast lag, replays events from the event store to recover any missed
/// terminal events.
///
/// The `shutdown_rx` receiver allows graceful shutdown of the listener.
pub fn spawn_automation_event_listener(
    event_bus: &EventBus,
    server: Arc<Server>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> JoinHandle<()> {
    let mut rx = event_bus.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(event) => {
                            handle_automation_event(&event, &server).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            error!(
                                skipped = n,
                                "automation event listener lagged — {n} events dropped, replaying from store"
                            );
                            recover_from_lag(&server.event_bus, &server).await;
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

/// Spawn a watchdog that periodically scans for automation runs stuck in
/// `Running` state past `hard_limit + buffer` and forcibly fails them.
///
/// This is a last-resort safety net: if the event listener misses terminal
/// events (e.g., due to broadcast lag, process restart, or bugs), the watchdog
/// ensures runs don't stay in `Running` forever.
pub fn spawn_stuck_run_watchdog(
    server: Arc<Server>,
    hard_limit: Duration,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> JoinHandle<()> {
    let cutoff_duration = hard_limit + STUCK_RUN_BUFFER;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(WATCHDOG_INTERVAL);

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown_rx.recv() => {
                    info!("stuck-run watchdog received shutdown signal");
                    break;
                }
            }

            let cutoff = chrono::Utc::now() - cutoff_duration;
            let stuck_runs = match server.list_stuck_automation_runs(&cutoff) {
                Ok(runs) => runs,
                Err(e) => {
                    error!(error = %e, "watchdog failed to query stuck runs");
                    continue;
                }
            };

            for run in stuck_runs {
                warn!(
                    run_id = %run.id,
                    started_at = %run.started_at,
                    "watchdog detected stuck automation run, forcibly failing"
                );
                if let Err(e) = server
                    .fail_automation_run(
                        &run.id,
                        format!(
                            "watchdog: run stuck in Running state past hard limit + {}s buffer",
                            STUCK_RUN_BUFFER.as_secs()
                        ),
                    )
                    .await
                {
                    error!(
                        run_id = %run.id,
                        error = %e,
                        "watchdog failed to mark stuck run as failed"
                    );
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
