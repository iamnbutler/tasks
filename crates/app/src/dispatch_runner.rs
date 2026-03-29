//! Dispatch runner — executes one-off orchestrator-dispatched agent sessions.
//!
//! When the orchestrator's `think()` pass returns a `DispatchAgent` action,
//! the run loop calls `execute_dispatch()` to start a short-lived container
//! session. Results feed back to the orchestrator via events.
//!
//! Modeled after `automation_runner.rs` but with shorter default time limits
//! and orchestrator-specific event types.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{error, info};
use uuid::Uuid;

use events::{Actor, Event, EventBus, EventType};
use runtime::AppleContainerRuntime;
use server::Server;
use tasks_orchestrator::DispatchRequest;
use tasks_session::{SessionLimits, SessionManager};

/// Prefix used for orchestrator dispatch session IDs.
const SESSION_PREFIX: &str = "orchestrator-dispatch:";

/// Default soft time limit for dispatch sessions (5 minutes).
const DEFAULT_SOFT_LIMIT_SECS: u64 = 300;

/// Default hard time limit for dispatch sessions (10 minutes).
const DEFAULT_HARD_LIMIT_SECS: u64 = 600;

/// Execute an orchestrator-dispatched agent session.
///
/// Creates a session with the ID `orchestrator-dispatch:{dispatch_id}`.
/// Emits `orchestrator:dispatch:started` on launch. Completion and failure
/// events are emitted by the companion event listener.
pub async fn execute_dispatch(
    session_manager: &SessionManager<AppleContainerRuntime>,
    server: &Arc<Server>,
    request: &DispatchRequest,
) {
    let dispatch_id = Uuid::new_v4().to_string()[..8].to_string();
    let session_id = format!("{SESSION_PREFIX}{dispatch_id}");

    // Resolve the project to get the repo URL.
    let repo_url = match server.get_project(&request.project_id).await {
        Some(p) => format!("https://github.com/{}.git", p.repo),
        None => {
            error!(
                dispatch_id = %dispatch_id,
                project_id = %request.project_id,
                "dispatch: project not found"
            );
            let event = Event::new(
                EventType::OrchestratorDispatchFailed,
                &session_id,
                Actor::Orchestrator,
                serde_json::json!({
                    "dispatch_id": dispatch_id,
                    "reason": format!("project not found: {}", request.project_id),
                }),
            );
            let _ = server.event_bus.publish(event).await;
            return;
        }
    };

    // Dispatch sessions work on a unique branch.
    let unique_suffix = &Uuid::new_v4().to_string()[..8];
    let branch = format!("orchestrator/{}--{}", dispatch_id, unique_suffix);

    let soft_limit = Duration::from_secs(
        request.soft_limit_secs.unwrap_or(DEFAULT_SOFT_LIMIT_SECS),
    );
    let hard_limit = Duration::from_secs(
        request.hard_limit_secs.unwrap_or(DEFAULT_HARD_LIMIT_SECS),
    );

    // Prepend context to the prompt
    let hard_limit_mins = hard_limit.as_secs() / 60;
    let full_prompt = format!(
        "[ORCHESTRATOR DISPATCH] You are an agent dispatched by the project orchestrator.\n\
         Reason: {reason}\n\
         Time limit: {hard_limit_mins} minutes.\n\
         Complete your work and exit promptly.\n\n\
         {prompt}",
        reason = request.reason,
        prompt = request.prompt,
    );

    info!(
        dispatch_id = %dispatch_id,
        project_id = %request.project_id,
        intent = ?request.intent,
        reason = %request.reason,
        soft_limit_secs = soft_limit.as_secs(),
        hard_limit_secs = hard_limit.as_secs(),
        "starting orchestrator dispatch session"
    );

    // Emit started event
    let started_event = Event::new(
        EventType::OrchestratorDispatchStarted,
        &session_id,
        Actor::Orchestrator,
        serde_json::json!({
            "dispatch_id": dispatch_id,
            "project_id": request.project_id,
            "intent": request.intent,
            "reason": request.reason,
        }),
    );
    if let Err(e) = server.event_bus.publish(started_event).await {
        error!(error = %e, "failed to publish dispatch started event");
    }

    let time_limits = SessionLimits {
        soft_limit: Some(soft_limit),
        hard_limit: Some(hard_limit),
    };

    if let Err(e) = session_manager
        .start_session_with_limits(
            session_id.clone(),
            repo_url,
            branch,
            full_prompt,
            None,
            None,
            time_limits,
        )
        .await
    {
        error!(
            dispatch_id = %dispatch_id,
            error = %e,
            "failed to start dispatch session"
        );
        let event = Event::new(
            EventType::OrchestratorDispatchFailed,
            &session_id,
            Actor::Orchestrator,
            serde_json::json!({
                "dispatch_id": dispatch_id,
                "reason": format!("failed to start session: {e}"),
            }),
        );
        let _ = server.event_bus.publish(event).await;
    }
}

/// Returns `true` if the session ID belongs to an orchestrator dispatch.
pub fn is_dispatch_session(session_id: &str) -> bool {
    session_id.starts_with(SESSION_PREFIX)
}

/// Extract the dispatch ID from a dispatch session ID.
pub fn dispatch_id_from_session(session_id: &str) -> Option<&str> {
    session_id.strip_prefix(SESSION_PREFIX)
}

/// Spawn a background task that listens for dispatch session completion/failure
/// events and emits the corresponding orchestrator dispatch events.
pub fn spawn_dispatch_event_listener(
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
                            // Only care about dispatch sessions
                            let dispatch_id = match dispatch_id_from_session(&event.task) {
                                Some(id) => id.to_string(),
                                None => continue,
                            };

                            match event.event_type {
                                // Agent exited successfully
                                events::EventType::TaskStateCompleted
                                | events::EventType::TaskStateAwaitingMerge => {
                                    info!(
                                        dispatch_id = %dispatch_id,
                                        event_type = %event.event_type.as_str(),
                                        "dispatch session completed"
                                    );
                                    let completed = Event::new(
                                        EventType::OrchestratorDispatchCompleted,
                                        &event.task,
                                        Actor::Orchestrator,
                                        serde_json::json!({
                                            "dispatch_id": dispatch_id,
                                        }),
                                    );
                                    if let Err(e) = server.event_bus.publish(completed).await {
                                        error!(
                                            dispatch_id = %dispatch_id,
                                            error = %e,
                                            "failed to publish dispatch completed event"
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
                                        dispatch_id = %dispatch_id,
                                        reason = %reason,
                                        "dispatch session failed"
                                    );
                                    let failed = Event::new(
                                        EventType::OrchestratorDispatchFailed,
                                        &event.task,
                                        Actor::Orchestrator,
                                        serde_json::json!({
                                            "dispatch_id": dispatch_id,
                                            "reason": reason,
                                        }),
                                    );
                                    if let Err(e) = server.event_bus.publish(failed).await {
                                        error!(
                                            dispatch_id = %dispatch_id,
                                            error = %e,
                                            "failed to publish dispatch failed event"
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            error!(
                                skipped = n,
                                "dispatch event listener lagged — {n} events dropped"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!("event bus closed, dispatch event listener shutting down");
                            break;
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("dispatch event listener received shutdown signal");
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
    fn test_is_dispatch_session() {
        assert!(is_dispatch_session("orchestrator-dispatch:abc-123"));
        assert!(!is_dispatch_session("automation-run:abc-123"));
        assert!(!is_dispatch_session("task-abc-123"));
        assert!(!is_dispatch_session(""));
    }

    #[test]
    fn test_dispatch_id_from_session() {
        assert_eq!(
            dispatch_id_from_session("orchestrator-dispatch:abc-123"),
            Some("abc-123")
        );
        assert_eq!(dispatch_id_from_session("task-abc-123"), None);
        assert_eq!(dispatch_id_from_session("orchestrator-dispatch:"), Some(""));
    }
}
