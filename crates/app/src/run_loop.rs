//! Main run loop — wires all components together.
//!
//! This is intentionally thin — the logic lives in the library crates.

use std::sync::Arc;

use tokio::sync::RwLock;

use events::{Actor, Event, EventBus, EventStore, EventType};
use runtime::{AppleContainerRuntime, ContainerConfig};
use server::model::project::Project;
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
    eprintln!("Tasks platform starting...");
    eprintln!("  max_sessions: {}", config.max_sessions);
    eprintln!("  poll_interval: {:?}", config.poll_interval);
    eprintln!("  dispatch_interval: {:?}", config.dispatch_interval);
    eprintln!("  container_image: {}", config.container_image);

    // --- 1. Create infrastructure ---

    // Event store — use a temp directory for now. Persistence is future work.
    let event_dir = std::env::var("TASKS_EVENT_DIR").unwrap_or_else(|_| {
        let dir = std::env::temp_dir().join("tasks-events");
        std::fs::create_dir_all(&dir).ok();
        dir.to_string_lossy().to_string()
    });
    let store = EventStore::new(&event_dir);
    let bus = EventBus::new(store, 256);

    // --- 2. Create server ---

    let server = Arc::new(Server::new(bus));

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

    // --- 4. Register projects and create pollers ---

    let projects_str = std::env::var("TASKS_PROJECTS").unwrap_or_default();

    let mut pollers: Vec<(String, RepoPoller)> = Vec::new();

    for repo_ref in projects_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let parts: Vec<&str> = repo_ref.split('/').collect();
        if parts.len() != 2 {
            eprintln!("Warning: invalid project reference: {repo_ref}");
            continue;
        }
        let (owner, repo) = (parts[0], parts[1]);
        let project_id = repo_ref.to_string();

        let project = Project::new(&project_id, repo_ref);
        server.add_project(project).await;

        let client = GitHubClient::new(&config.github_token);
        let poller = RepoPoller::new(client, owner, repo);
        pollers.push((project_id, poller));
    }

    // --- 5. Emit system:started ---

    server.emit_started().await?;
    eprintln!("Tasks platform started ({} projects)", pollers.len());

    // --- 6. Spawn GitHub poll loop ---

    let poll_server = server.clone();
    let poll_interval = config.poll_interval;
    let pollers = Arc::new(RwLock::new(pollers));
    let poll_pollers = pollers.clone();

    let poll_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(poll_interval);
        let label_config = server::workflow::LabelConfig::default();

        loop {
            interval.tick().await;

            let mut pollers = poll_pollers.write().await;
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
                                        eprintln!(
                                            "Failed to add task for issue #{}: {e}",
                                            issue.number
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
                                        eprintln!(
                                            "Failed to add task for PR #{}: {e}",
                                            pr.number
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Poll error for {project_id}: {e}");
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
            let _ = poll_server.event_bus.publish(event).await;
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

                            // Build a basic prompt (full prompt construction is in server::prompt)
                            let prompt = format!(
                                "Implement: {} — {}",
                                task.title,
                                task.description.as_deref().unwrap_or("No description")
                            );

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
                                eprintln!("Failed to start session for {task_id}: {e}");
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
                            eprintln!("Failed to resume session for {task_id}: {e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Dispatch error: {e}");
                }
            }
        }
    });

    // --- 8. Wait for shutdown ---

    tokio::signal::ctrl_c().await?;
    eprintln!("\nShutting down...");

    // Stop all sessions
    session_manager.stop_all().await;

    // Cancel the loops
    poll_handle.abort();
    dispatch_handle.abort();

    eprintln!("Shutdown complete.");
    Ok(())
}
