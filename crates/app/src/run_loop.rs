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

use tasks_orchestrator::Orchestrator;

use crate::config::AppConfig;
use crate::memory::{MemoryGate, MemoryThresholds};

/// Enum wrapper for orchestrator implementations.
///
/// `trait_variant::make(Send)` generates non-dyn-compatible traits, so we
/// use an enum to dispatch instead of `Arc<dyn Orchestrator>`.
enum AnyOrchestrator {
    Claude(tasks_orchestrator::ClaudeOrchestrator),
    Mock(tasks_orchestrator::MockOrchestrator),
}

impl AnyOrchestrator {
    async fn evaluate(
        &self,
        context: &tasks_orchestrator::EvaluationContext,
    ) -> Result<tasks_orchestrator::QualityEvaluation, tasks_orchestrator::OrchestratorError> {
        match self {
            Self::Claude(o) => o.evaluate(context).await,
            Self::Mock(o) => o.evaluate(context).await,
        }
    }
}

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
        container_memory = %config.container_memory,
        progress_threshold = ?config.progress_threshold,
        memory_warn_pct = config.memory_warn_pct,
        memory_soft_limit_pct = config.memory_soft_limit_pct,
        memory_hard_limit_pct = config.memory_hard_limit_pct,
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

    // --- 3. Create container runtime ---

    let container_runtime = AppleContainerRuntime::new();

    // --- 4. Restart recovery (spec §13.3) ---
    //
    // Detect orphaned sessions from previous run and recover them.
    // This must happen after load_from_store and before starting the dispatch loop.
    {
        let state = server.state.read().await;
        let recovery_result = server::recovery::detect_orphaned_sessions(
            &state.tasks,
            &container_runtime,
            config.max_retries,
        )
        .await;
        drop(state);

        if !recovery_result.retried.is_empty() || !recovery_result.failed.is_empty() {
            info!(
                retried = recovery_result.retried.len(),
                failed = recovery_result.failed.len(),
                alive = recovery_result.alive.len(),
                "recovering orphaned sessions from previous run"
            );
            server.apply_recovery_result(&recovery_result).await?;
        }
    }

    // --- 5. Create session manager ---
    let mut default_container_config = ContainerConfig::new(&config.container_image)
        .env("GITHUB_TOKEN", &config.github_token)
        .memory(&config.container_memory);
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
        .with_hard_time_limit(config.session_hard_limit)
        .with_progress_threshold(config.progress_threshold),
    );

    // --- 5b. Create orchestrator ---

    let orchestrator = Arc::new(match tasks_orchestrator::ClaudeOrchestrator::from_env() {
        Ok(o) => {
            info!("orchestrator initialized (Claude-backed)");
            AnyOrchestrator::Claude(o)
        }
        Err(e) => {
            warn!(error = %e, "failed to initialize Claude orchestrator, using mock (always rejects)");
            AnyOrchestrator::Mock(tasks_orchestrator::MockOrchestrator::rejecting(
                "orchestrator not configured",
            ))
        }
    });

    // --- 5c. Create memory watchdog ---

    let memory_thresholds = MemoryThresholds {
        warn_pct: config.memory_warn_pct,
        soft_limit_pct: config.memory_soft_limit_pct,
        hard_limit_pct: config.memory_hard_limit_pct,
    };
    let memory_gate = Arc::new(MemoryGate::new());

    // Log initial memory state
    {
        let snapshot = crate::memory::sample_memory(&memory_thresholds);
        info!(
            total_gb = snapshot.total_bytes / (1024 * 1024 * 1024),
            used_pct = snapshot.used_pct,
            warn_pct = config.memory_warn_pct,
            soft_limit_pct = config.memory_soft_limit_pct,
            hard_limit_pct = config.memory_hard_limit_pct,
            "memory watchdog configured"
        );
    }

    let watchdog_gate = memory_gate.clone();
    let watchdog_bus = server.event_bus.clone();
    let watchdog_sessions = session_manager.clone();

    let watchdog_handle = tokio::spawn(async move {
        crate::memory::watchdog_loop(
            watchdog_gate,
            memory_thresholds,
            watchdog_bus,
            watchdog_sessions,
            std::time::Duration::from_secs(10),
        )
        .await;
    });

    // --- 6. Emit system:started ---

    let project_count = server.state.read().await.projects.len();
    server.emit_started().await?;
    info!(projects = project_count, "tasks platform started");

    // --- 7. Spawn GitHub poll loop ---
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
                        // TODO: Re-enable PR task creation and merge queue population
                        // once the core spec is more complete. Currently causes loops
                        // where agent-created PRs get picked up as new tasks, which
                        // then create more PRs, etc.
                        //
                        // for pr in &result.pull_requests {
                        //     let source = TaskSource::GithubPr { ... };
                        //     // Create tasks for new PRs
                        //     // Add open PRs to the merge queue
                        // }
                        let _ = &result.pull_requests; // suppress unused warning
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

    // --- 7b. Spawn event handler loop ---
    //
    // Listens for session lifecycle events and feeds state changes back
    // into the server. The session monitor publishes events (e.g.
    // TaskStateAwaitingMerge) but doesn't update server state directly.
    // This loop bridges events → state updates + merge queue entries.
    //
    // For TaskStateFailed events from agents, we use progress detection
    // (spec §13.1, §13.2) to decide whether to retry or mark as failed.

    let event_handler_server = server.clone();
    let event_handler_bus = server.event_bus.clone();
    let event_handler_max_retries = config.max_retries;

    let event_handler_handle = tokio::spawn(async move {
        let mut rx = event_handler_bus.subscribe();
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

            // Handle TaskStateFailed specially: use progress detection (spec §13.1)
            // to determine whether to retry or mark as permanently failed.
            if event.event_type == EventType::TaskStateFailed
                && event.actor != events::Actor::Scheduler
                && event.actor != events::Actor::System
            {
                // Extract made_progress from event data (defaults to false)
                let made_progress = event.data.get("made_progress")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if let Err(e) = event_handler_server
                    .handle_task_failure(task_id, made_progress, event_handler_max_retries)
                    .await
                {
                    if !matches!(e, server::ServerError::TaskNotFound(_)) {
                        error!(
                            task_id = %task_id,
                            made_progress = made_progress,
                            error = %e,
                            "failed to handle task failure"
                        );
                    }
                }
                continue;
            }

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
            }
        }
    });

    // --- 8. Spawn dispatch tick loop ---

    let dispatch_server = server.clone();
    let dispatch_session_mgr = session_manager.clone();
    let dispatch_interval = config.dispatch_interval;
    let max_sessions = config.max_sessions;
    let dispatch_event_bus = server.event_bus.clone();
    let dispatch_memory_gate = memory_gate.clone();

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

            // Check memory pressure before dispatching new work.
            // When paused, pass global_max=0 so the dispatcher won't select
            // new work (which would transition tasks to Running prematurely),
            // but resumes still go through.
            let memory_paused = dispatch_memory_gate.is_dispatch_paused();
            let effective_max = if memory_paused {
                let pct = dispatch_memory_gate.current_pct.load(std::sync::atomic::Ordering::Relaxed);
                warn!(
                    used_pct = pct,
                    "dispatch: no new sessions due to memory pressure"
                );
                0
            } else {
                max_sessions
            };

            // Run dispatch
            let pending_answers: Vec<String> = Vec::new(); // TODO: track pending answers
            match dispatch_server
                .run_dispatch(&pending_answers, effective_max)
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

    // --- 8b. Spawn orchestrator loop ---
    //
    // The orchestrator is the project foreman. On each tick it observes
    // project state and acts: evaluates merge queue entries, comments on
    // PRs, adjusts tasks. Currently only merge queue evaluation is wired.
    // In Play mode it auto-approves/rejects. In Pause mode it evaluates
    // but doesn't merge. In Stop mode it's idle.

    let orch_server = server.clone();
    let orch = orchestrator.clone();
    let orch_event_bus = server.event_bus.clone();
    let orchestrator_interval = config.dispatch_interval; // reuse dispatch interval for now

    let orchestrator_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(orchestrator_interval);
        let mut event_rx = orch_event_bus.subscribe();

        loop {
            // Tick on interval or on merge-relevant events
            tokio::select! {
                _ = interval.tick() => {},
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => {
                            if !matches!(
                                event.event_type,
                                EventType::MergeQueued
                                | EventType::SystemModePlay
                                | EventType::SystemModePause
                            ) {
                                continue;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "orchestrator loop lagged");
                            continue;
                        }
                        Err(_) => break, // channel closed, shut down
                    }
                }
            };

            // Read current mode — idle in Stop
            let mode = orch_server.mode().await;
            if mode == server::Mode::Stop {
                continue;
            }

            // Snapshot pending merge queue entries with their tasks and projects
            let pending: Vec<(String, String, String)> = {
                let state = orch_server.state.read().await;
                state.merge_queue.pending().iter().map(|entry| {
                    (entry.id.clone(), entry.task_id.clone(), entry.pr_url.clone())
                }).collect()
            };

            for (entry_id, task_id, _pr_url) in pending {
                // Build evaluation context
                let (task, project) = {
                    let state = orch_server.state.read().await;
                    let task = match state.tasks.get(&task_id) {
                        Some(t) => t.clone(),
                        None => continue,
                    };
                    let project = match state.projects.get(&task.project) {
                        Some(p) => p.clone(),
                        None => continue,
                    };
                    (task, project)
                };

                let entry = {
                    let state = orch_server.state.read().await;
                    match state.merge_queue.get(&entry_id) {
                        Some(e) => e.clone(),
                        None => continue,
                    }
                };

                let context = tasks_orchestrator::EvaluationContext {
                    entry,
                    task: task.clone(),
                    project,
                };

                // Evaluate
                let evaluation = match orch.evaluate(&context).await {
                    Ok(eval) => eval,
                    Err(e) => {
                        error!(
                            entry_id = %entry_id,
                            task_id = %task_id,
                            error = %e,
                            "orchestrator evaluation failed"
                        );
                        continue;
                    }
                };

                // Always emit a decision event (audit trail)
                if let Err(e) = orch_server.emit_orchestrator_decision(
                    &task_id,
                    &entry_id,
                    evaluation.approved,
                    &evaluation.reasoning,
                ).await {
                    error!(error = %e, "failed to emit orchestrator decision event");
                }

                // In Play mode: act on the decision
                if mode == server::Mode::Play {
                    if evaluation.approved {
                        if let Err(e) = orch_server.approve_merge_entry(&entry_id, &evaluation.reasoning).await {
                            error!(entry_id = %entry_id, error = %e, "failed to approve merge entry");
                        }
                    } else {
                        if let Err(e) = orch_server.reject_merge_entry(
                            &entry_id,
                            &evaluation.reasoning,
                            evaluation.feedback.as_deref(),
                        ).await {
                            error!(entry_id = %entry_id, error = %e, "failed to reject merge entry");
                        }
                        // TODO: Call orch.feedback(&task, feedback) to deliver
                        // guidance to the re-dispatched session. Currently the
                        // feedback only lands in the event payload; the fresh
                        // session started by the dispatch loop doesn't receive it.
                        // Needs dispatch loop changes to pass feedback as prompt context.
                    }
                }

                // In Pause mode: evaluation is recorded (decision event above)
                // but no merge/reject action is taken. The human reviews.
            }
        }
    });

    // --- 9. Optionally spawn web server ---

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

    // --- 10. Wait for shutdown (TUI or headless) ---

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
    orchestrator_handle.abort();
    watchdog_handle.abort();
    if let Some(h) = web_handle {
        h.abort();
    }

    info!("shutdown complete");
    Ok(())
}
