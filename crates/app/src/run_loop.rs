//! Main run loop — wires all components together.
//!
//! This is intentionally thin — the logic lives in the library crates.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::{error, info, warn};

use events::{Actor, Event, EventBus, EventStore, EventType};
use runtime::{AppleContainerRuntime, ContainerConfig};
use server::model::task::TaskSource;
use server::Server;
use tasks_github::client::GitHubClient;
use tasks_github::poller::RepoPoller;

use crate::config::AppConfig;

/// Run the Tasks platform.
///
/// Constructs all components and starts the GitHub poll loop,
/// dispatch tick loop, and session management.
pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    info!(
        data_dir = %config.data_dir,
        max_sessions = config.max_sessions,
        poll_interval = ?config.poll_interval,
        dispatch_interval = ?config.dispatch_interval,
        container_image = %config.container_image,
        "starting tasks platform"
    );

    // --- 1. Create infrastructure ---

    std::fs::create_dir_all(&config.data_dir)?;

    let db_path = format!("{}/db.sqlite", config.data_dir);
    let store = tasks_store::Store::open(&db_path)?;

    let event_dir = format!("{}/events", config.data_dir);
    std::fs::create_dir_all(&event_dir)?;
    let event_store = EventStore::new(&event_dir);
    let bus = EventBus::new(event_store, 256);

    // --- 2. Create server ---

    let server = Arc::new(Server::with_store(bus, store));
    server
        .load_from_store()
        .await
        .map_err(|e| format!("Failed to load state: {e}"))?;

    // --- 3. Create session manager ---

    let container_runtime = AppleContainerRuntime::new();
    let mut default_container_config =
        ContainerConfig::new(&config.container_image).env("GITHUB_TOKEN", &config.github_token);
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        default_container_config = default_container_config.env("ANTHROPIC_API_KEY", key);
    }

    let session_manager = Arc::new(
        tasks_session::SessionManager::new(
            container_runtime,
            server.event_bus.clone(),
            default_container_config,
        )
        .with_soft_time_limit(config.session_soft_limit)
        .with_hard_time_limit(config.session_hard_limit),
    );

    // --- 4. Emit system:started ---

    let project_count = server.state.read().await.projects.len();
    server.emit_started().await?;
    info!(projects = project_count, "tasks platform started");

    // --- 5. Spawn GitHub poll loop ---
    //
    // Pollers are rebuilt from the live project list each tick so that
    // projects added/removed via the web UI take effect immediately.

    let poll_server = server.clone();
    let poll_interval = config.poll_interval;
    let github_token = config.github_token.clone();

    let poll_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(poll_interval);
        let label_config = server::workflow::LabelConfig::default();
        let mut pollers: HashMap<String, RepoPoller> = HashMap::new();

        loop {
            interval.tick().await;

            // Sync pollers with live project list
            let projects: Vec<(String, String)> = {
                let state = poll_server.state.read().await;
                state.projects.iter().map(|(id, p)| (id.clone(), p.repo.clone())).collect()
            };

            // Remove pollers for deleted projects
            let active_ids: std::collections::HashSet<&str> = projects.iter().map(|(id, _)| id.as_str()).collect();
            pollers.retain(|id, _| active_ids.contains(id.as_str()));

            // Add pollers for new projects
            for (project_id, repo) in &projects {
                if !pollers.contains_key(project_id) {
                    let parts: Vec<&str> = repo.split('/').collect();
                    if parts.len() == 2 {
                        let client = GitHubClient::new(&github_token);
                        let poller = RepoPoller::new(client, parts[0], parts[1]);
                        pollers.insert(project_id.clone(), poller);
                    }
                }
            }

            for (project_id, poller) in pollers.iter_mut() {
                match poller.poll().await {
                    Ok(result) => {
                        // Create tasks for new issues
                        for issue in &result.issues {
                            let source = TaskSource::GithubIssue {
                                owner: issue.owner.clone(),
                                repo: issue.repo.clone(),
                                number: issue.number,
                            };
                            if !poll_server.has_task_for_source(&source).await {
                                if let Some(task) = server::scheduler::issue_to_task(
                                    issue,
                                    project_id,
                                    &label_config,
                                ) {
                                    if let Err(e) = poll_server.add_task(task).await {
                                        warn!(
                                            project = %project_id,
                                            issue = issue.number,
                                            error = %e,
                                            "failed to add task for issue"
                                        );
                                    }
                                }
                            }
                        }
                        // Create tasks for new PRs
                        for pr in &result.pull_requests {
                            let source = TaskSource::GithubPr {
                                owner: pr.owner.clone(),
                                repo: pr.repo.clone(),
                                number: pr.number,
                            };
                            if !poll_server.has_task_for_source(&source).await {
                                if let Some(task) = server::scheduler::pr_to_task(
                                    pr,
                                    project_id,
                                    &label_config,
                                ) {
                                    if let Err(e) = poll_server.add_task(task).await {
                                        warn!(
                                            project = %project_id,
                                            pr = pr.number,
                                            error = %e,
                                            "failed to add task for PR"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(project = %project_id, error = %e, "poll failed");
                    }
                }
            }

            // Emit scheduler tick event
            let event = Event::new(
                EventType::SystemSchedulerTick,
                "system",
                Actor::Scheduler,
                serde_json::json!({}),
            );
            if let Err(e) = poll_server.event_bus.publish(event).await {
                error!(error = %e, "failed to publish scheduler tick event");
            }
        }
    });

    // --- 6b. Spawn event handler loop ---
    //
    // Listens for session lifecycle events and feeds state changes back
    // into the server. The session monitor publishes events (e.g.
    // TaskStateAwaitingMerge) but doesn't update server state directly.
    // This loop bridges events → state updates + merge queue entries.

    let event_handler_server = server.clone();
    let event_handler_bus = server.event_bus.clone();
    let event_handler_github_token = config.github_token.clone();

    let event_handler_handle = tokio::spawn(async move {
        let mut rx = event_handler_bus.subscribe();
        let github = GitHubClient::new(&event_handler_github_token);
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "event handler lagged, some events may not update state");
                    continue;
                }
                Err(_) => break,
            };

            let task_id = &event.task;
            let new_state = match event.event_type {
                EventType::TaskStateRunning => Some(models::task::TaskState::Running),
                EventType::TaskStateQuestion => Some(models::task::TaskState::Question),
                EventType::TaskStateWaiting => Some(models::task::TaskState::Waiting),
                EventType::TaskStateBlocked => Some(models::task::TaskState::Blocked),
                EventType::TaskStateTesting => Some(models::task::TaskState::Testing),
                EventType::TaskStateAwaitingMerge => Some(models::task::TaskState::AwaitingMerge),
                EventType::TaskStateConflict => Some(models::task::TaskState::Conflict),
                EventType::TaskStateCompleted => Some(models::task::TaskState::Completed),
                EventType::TaskStateFailed => Some(models::task::TaskState::Failed),
                EventType::TaskStateCancelled => Some(models::task::TaskState::Cancelled),
                _ => None,
            };

            if let Some(state) = new_state {
                // Only process events from agents/sessions — skip events
                // already published by set_task_state (from scheduler/dispatch)
                // to avoid double-applying state and bumping updated_at twice.
                if event.actor != events::Actor::Scheduler {
                    if let Err(e) = event_handler_server.apply_task_state(task_id, state).await {
                        if !matches!(e, server::ServerError::TaskNotFound(_)) {
                            error!(task_id = %task_id, error = %e, "failed to update task state from event");
                        }
                    }
                }

                // Enqueue completed task for merge review (spec §7.1).
                // The merge queue is task-centric — a PR is just one possible
                // artifact an agent may produce. We do a best-effort lookup
                // for a PR on the task branch, but the entry is valid without one.
                // TODO: orchestrator quality gate before enqueuing (spec §7.3)
                if matches!(event.event_type, EventType::TaskStateAwaitingMerge) {
                    let entry_id = uuid::Uuid::new_v4().to_string();
                    let mut entry = models::merge_queue::MergeQueueEntry::new(&entry_id, task_id);

                    // Best-effort: check if the agent opened a PR for this task's branch
                    if let Some(task) = event_handler_server.get_task(task_id).await {
                        if let Some(project) = event_handler_server.get_project(&task.project).await {
                            let parts: Vec<&str> = project.repo.split('/').collect();
                            if parts.len() == 2 {
                                let head = format!("tasks/{}", task.id);
                                match github.find_pr_for_branch(parts[0], parts[1], &head).await {
                                    Ok(Some(url)) => {
                                        info!(task_id = %task_id, pr_url = %url, "found PR for task branch");
                                        entry.pr_url = Some(url);
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        warn!(task_id = %task_id, error = %e, "failed to query PRs for task branch");
                                    }
                                }
                            }
                        }
                    }

                    info!(task_id = %task_id, entry_id = %entry_id, "enqueuing task for merge review");
                    if let Err(e) = event_handler_server.add_to_merge_queue(entry).await {
                        error!(task_id = %task_id, error = %e, "failed to add merge queue entry");
                    }
                }
            }
        }
    });

    // --- 7. Spawn dispatch tick loop ---

    let dispatch_server = server.clone();
    let dispatch_session_mgr = session_manager.clone();
    let dispatch_interval = config.dispatch_interval;
    let max_sessions = config.max_sessions;
    let dispatch_event_bus = server.event_bus.clone();

    let dispatch_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(dispatch_interval);
        let mut event_rx = dispatch_event_bus.subscribe();

        loop {
            // Wait for either the tick or a dispatch-triggering event
            let should_dispatch = tokio::select! {
                _ = interval.tick() => true,
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => matches!(
                            event.event_type,
                            EventType::TaskCreated
                            | EventType::TaskStateCompleted
                            | EventType::TaskStateFailed
                            | EventType::TaskStateCancelled
                            | EventType::TaskStateWaiting
                            | EventType::SystemModePause
                            | EventType::SystemModePlay
                        ),
                        Err(_) => false,
                    }
                }
            };

            if !should_dispatch {
                continue;
            }

            // Run dispatch
            let pending_answers: Vec<String> = Vec::new(); // TODO: track pending answers
            match dispatch_server
                .run_dispatch(&pending_answers, max_sessions)
                .await
            {
                Ok(plan) => {
                    // Start new sessions for dispatched tasks
                    for task_id in &plan.new_work {
                        if let Some(task) = dispatch_server.get_task(task_id).await {
                            let project = dispatch_server.get_project(&task.project).await;
                            let repo_url = project
                                .as_ref()
                                .map(|p| format!("https://github.com/{}.git", p.repo))
                                .unwrap_or_default();
                            let branch = format!("tasks/{}", task.id);

                            let prompt = server::prompt::build_prompt_for_task(&task, &branch);

                            if let Err(e) = dispatch_session_mgr
                                .start_session(
                                    task_id.clone(),
                                    repo_url,
                                    branch,
                                    prompt,
                                    None,
                                )
                                .await
                            {
                                error!(task_id = %task_id, error = %e, "failed to start session");
                                // Transition back to Waiting so dispatcher can retry.
                                if let Err(e2) = dispatch_server
                                    .set_task_state(
                                        task_id,
                                        models::task::TaskState::Waiting,
                                        events::Actor::System,
                                    )
                                    .await
                                {
                                    warn!(task_id = %task_id, error = %e2, "failed to revert task to waiting — task may be stuck");
                                }
                            }
                        }
                    }

                    // Resume sessions with pending answers
                    for task_id in &plan.resume {
                        // TODO: look up the pending message and send it
                        if let Err(e) = dispatch_session_mgr
                            .send_chat(task_id, "Resuming — please continue.".to_string())
                            .await
                        {
                            error!(task_id = %task_id, error = %e, "failed to resume session");
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "dispatch failed");
                }
            }
        }
    });

    // --- 8. Optionally spawn web server ---

    let web_handle = if config.web {
        let api_state = crate::web::ApiState {
            server: server.clone(),
            max_sessions: config.max_sessions,
            session_manager: Some(session_manager.clone()),
        };
        let web_port = config.web_port;

        // Serve the built frontend from web/build if it exists,
        // otherwise just serve the API.
        let app = {
            let api_router = crate::web::router(api_state);
            let web_dir = std::env::current_dir()
                .unwrap_or_default()
                .join("web")
                .join("build");
            if web_dir.exists() {
                let serve = tower_http::services::ServeDir::new(&web_dir)
                    .fallback(tower_http::services::ServeFile::new(web_dir.join("index.html")));
                api_router.fallback_service(serve)
            } else {
                api_router
            }
        };

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{web_port}")).await?;
        info!(port = web_port, "web server started");
        Some(tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        }))
    } else {
        None
    };

    // --- 9. Wait for shutdown (TUI or headless) ---

    if config.tui {
        crate::tui::run_tui(server.clone(), server.event_bus.clone(), config.max_sessions).await?;
    } else {
        tokio::signal::ctrl_c().await?;
    }
    info!("shutting down");

    // Stop all sessions
    session_manager.stop_all().await;

    // Cancel the loops
    poll_handle.abort();
    dispatch_handle.abort();
    event_handler_handle.abort();
    if let Some(h) = web_handle {
        h.abort();
    }

    info!("shutdown complete");
    Ok(())
}
