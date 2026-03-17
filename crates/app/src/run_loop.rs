//! Main run loop — wires all components together.
//!
//! This is intentionally thin — the logic lives in the library crates.

use std::collections::HashMap;
use std::sync::Arc;

use std::sync::Mutex as StdMutex;

use tracing::{error, info, warn};

use events::{Actor, Event, EventBus, EventStore, EventType};
use runtime::{AppleContainerRuntime, ContainerConfig};
use server::Server;
use server::model::merge_queue::MergeQueueEntry;
use server::model::task::{TaskSource, TaskState};
use tasks_github::client::GitHubClient;
use tasks_github::model::IssueState;
use tasks_github::poller::RepoPoller;

use tasks_orchestrator::Orchestrator;

use crate::config::AppConfig;
use crate::memory::{MemoryGate, MemoryThresholds};
use crate::problem_tracker::ProblemTracker;

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

/// Lower operating mode from Play to Pause (spec §6.4).
///
/// Called by the orchestrator loop when problem patterns are detected.
/// Logs the reason and emits an escalation event.
async fn lower_mode(server: &Arc<server::Server>, reason: &crate::problem_tracker::LowerReason) {
    // Only lower from Play — if already in Pause/Stop, nothing to do
    let current_mode = server.mode().await;
    if current_mode != server::Mode::Play {
        return;
    }

    warn!(reason = %reason, "orchestrator lowering mode to Pause");

    // Lower mode
    if let Err(e) = server
        .set_mode(server::Mode::Pause, &events::Actor::Orchestrator)
        .await
    {
        error!(error = %e, "failed to lower mode");
        return;
    }

    // Emit escalation event so human knows why mode was lowered
    let event = events::Event::new(
        events::EventType::OrchestratorEscalation,
        "system",
        events::Actor::Orchestrator,
        serde_json::json!({
            "action": "mode_lowered",
            "from": "play",
            "to": "pause",
            "reason": reason.to_string(),
        }),
    );
    if let Err(e) = server.event_bus.publish(event).await {
        error!(error = %e, "failed to emit escalation event for mode lowering");
    }

    info!("mode lowered to Pause by orchestrator");
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
                state
                    .projects
                    .iter()
                    .map(|(id, p)| (id.clone(), p.repo.clone()))
                    .collect()
            };

            // Remove pollers for deleted projects
            let active_ids: std::collections::HashSet<&str> =
                projects.iter().map(|(id, _)| id.as_str()).collect();
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
                        // Add open PRs to the merge queue (spec §7).
                        //
                        // We don't create tasks for PRs here — that would cause loops
                        // where agent-created PRs get picked up as new tasks. Instead,
                        // we only add PRs to the merge queue, which is PR-centric.
                        //
                        // If a PR's branch matches the `tasks/{task_id}` pattern, we
                        // link it to that task. Otherwise, we use an empty task_id.
                        for pr in &result.pull_requests {
                            // Skip draft PRs — they aren't merge-ready
                            if pr.is_draft {
                                continue;
                            }

                            let pr_url = format!(
                                "https://github.com/{}/{}/pull/{}",
                                pr.owner, pr.repo, pr.number
                            );

                            // Skip if already in merge queue
                            if poll_server.has_merge_entry_for_pr(&pr_url).await {
                                continue;
                            }

                            // Try to find a linked task by branch name
                            let task_id = poll_server
                                .find_task_by_branch(&pr.head_ref)
                                .await
                                .unwrap_or_default();

                            // Create merge queue entry
                            let entry_id = format!("mq-{}-{}-pr-{}", pr.owner, pr.repo, pr.number);
                            let entry = MergeQueueEntry::new(
                                entry_id.clone(),
                                task_id.clone(),
                                &pr_url,
                            );

                            if let Err(e) = poll_server.add_to_merge_queue(entry).await {
                                warn!(
                                    project = %project_id,
                                    pr = pr.number,
                                    error = %e,
                                    "failed to add PR to merge queue"
                                );
                            } else {
                                info!(
                                    project = %project_id,
                                    pr = pr.number,
                                    task_id = %task_id,
                                    "added PR to merge queue"
                                );
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
                    warn!(
                        skipped = n,
                        "event handler lagged, some events may not update state"
                    );
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
    //
    // The dispatch loop is triggered by events (spec §12.1):
    // - task:created — new work available
    // - task:state:completed/failed/cancelled — slot freed up
    // - task:state:waiting — task became unblocked
    // - human:message — answer provided to Question-state task
    // - system:mode:pause/play — mode changed
    //
    // Plus a reconciliation tick to catch missed events (spec §12.1).
    // Uses debouncing to coalesce rapid events before dispatch.

    let dispatch_server = server.clone();
    let dispatch_session_mgr = session_manager.clone();
    let dispatch_interval = config.dispatch_interval;
    let max_sessions = config.max_sessions;
    let max_retries = config.max_retries;
    let dispatch_event_bus = server.event_bus.clone();
    let dispatch_memory_gate = memory_gate.clone();
    let dispatch_github_token = config.github_token.clone();

    // Debounce interval to coalesce rapid events (100ms)
    let debounce_duration = std::time::Duration::from_millis(100);

    let dispatch_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(dispatch_interval);
        let mut event_rx = dispatch_event_bus.subscribe();

        // Track pending answers: task IDs that received HumanMessage while in Question state
        let mut pending_answers: std::collections::HashSet<String> = std::collections::HashSet::new();

        // GitHub client for fetching comments at dispatch time (spec §15.2)
        let github_client = GitHubClient::new(&dispatch_github_token);

        loop {
            // Wait for either the tick or a dispatch-triggering event
            let trigger_event = tokio::select! {
                _ = interval.tick() => None,
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => Some(event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "dispatch loop lagged, some events may not trigger dispatch");
                            None
                        }
                        Err(_) => continue, // channel closed, will be handled by outer loop
                    }
                }
            };

            // Check if this is a dispatch-triggering event
            let should_dispatch = match &trigger_event {
                None => true, // reconciliation tick
                Some(event) => {
                    // Track HumanMessage for Question-state tasks (spec §12.1)
                    if event.event_type == EventType::HumanMessage {
                        // Check if task is in Question state
                        if let Some(task) = dispatch_server.get_task(&event.task).await {
                            if task.state == models::task::TaskState::Question {
                                pending_answers.insert(event.task.clone());
                                info!(task_id = %event.task, "tracking pending answer for dispatch");
                            }
                        }
                        true // trigger dispatch to process the answer
                    } else {
                        matches!(
                            event.event_type,
                            EventType::TaskCreated
                            | EventType::TaskStateCompleted
                            | EventType::TaskStateFailed
                            | EventType::TaskStateCancelled
                            | EventType::TaskStateWaiting
                            | EventType::SystemModePause
                            | EventType::SystemModePlay
                        )
                    }
                }
            };

            if !should_dispatch {
                continue;
            }

            // Debounce: wait briefly to coalesce rapid events (spec §12.1)
            // Continue collecting pending answers during debounce window
            let debounce_deadline = tokio::time::Instant::now() + debounce_duration;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(debounce_deadline) => break,
                    result = event_rx.recv() => {
                        if let Ok(event) = result {
                            // Track additional HumanMessage events during debounce
                            if event.event_type == EventType::HumanMessage {
                                if let Some(task) = dispatch_server.get_task(&event.task).await {
                                    if task.state == models::task::TaskState::Question {
                                        pending_answers.insert(event.task.clone());
                                    }
                                }
                            }
                        }
                    }
                }
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

            // Run dispatch with pending answers
            let answers_vec: Vec<String> = pending_answers.iter().cloned().collect();
            match dispatch_server
                .run_dispatch(&answers_vec, effective_max)
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

                            // Load workflow settings (spec §14, §15)
                            let workflow_settings = load_workflow_settings_for_project(
                                project.as_ref(),
                                &dispatch_github_token,
                            )
                            .await;

                            // Fetch comments from GitHub at dispatch time (spec §15.2)
                            let comments = server::prompt::fetch_comments_for_task(
                                &github_client,
                                &task.source,
                            )
                            .await;

                            let prompt = server::prompt::build_prompt_for_task(
                                &task,
                                &branch,
                                workflow_settings.system_prompt.as_deref(),
                                &comments,
                            );

                            if let Err(e) = dispatch_session_mgr
                                .start_session(
                                    task_id.clone(),
                                    repo_url,
                                    branch,
                                    prompt,
                                    None,
                                    workflow_settings.progress_threshold,
                                )
                                .await
                            {
                                error!(task_id = %task_id, error = %e, "failed to start session");
                                // Treat as a failure with no progress so backoff kicks in.
                                // This prevents tight retry loops when containers can't start.
                                if let Err(e2) = dispatch_server
                                    .handle_task_failure(task_id, false, max_retries)
                                    .await
                                {
                                    warn!(task_id = %task_id, error = %e2, "failed to handle session start failure");
                                }
                            }
                        }
                    }

                    // Resume sessions with pending answers — the message was already
                    // sent to the session when the HumanMessage event was emitted,
                    // so we just transition the task state here. The session manager
                    // handles delivering the message to the agent.
                    for task_id in &plan.resume {
                        info!(task_id = %task_id, "resumed session with pending answer");
                        // Remove from pending answers set — it's been processed
                        pending_answers.remove(task_id);
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
    //
    // Mode lowering (spec §6.4): The orchestrator tracks problem patterns
    // and can lower mode from Play to Pause when things go wrong.

    let orch_server = server.clone();
    let orch = orchestrator.clone();
    let orch_event_bus = server.event_bus.clone();
    let orch_github_token = config.github_token.clone();

    let orchestrator_interval = config.dispatch_interval; // reuse dispatch interval for now

    // Problem tracker for mode lowering (spec §6.4)
    let problem_tracker = Arc::new(StdMutex::new(ProblemTracker::new()));

    let orchestrator_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(orchestrator_interval);
        let mut event_rx = orch_event_bus.subscribe();
        let merge_github = GitHubClient::new(&orch_github_token);
        loop {
            // Tick on interval or on relevant events
            let event_opt = tokio::select! {
                _ = interval.tick() => None,
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => Some(event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "orchestrator loop lagged");
                            continue;
                        }
                        Err(_) => break, // channel closed, shut down
                    }
                }
            };

            // Handle specific events for problem tracking and mode lowering
            if let Some(ref event) = event_opt {
                match event.event_type {
                    // Reset problem tracker when human raises mode to Play
                    EventType::SystemModePlay => {
                        if let Ok(mut tracker) = problem_tracker.lock() {
                            tracker.reset();
                            info!("problem tracker reset (mode raised to Play)");
                        }
                    }
                    // Track agent errors
                    EventType::AgentError => {
                        let should_lower = {
                            let mut tracker = problem_tracker.lock().unwrap();
                            tracker.record_agent_error();
                            tracker.should_lower_mode()
                        };
                        if let Some(reason) = should_lower {
                            lower_mode(&orch_server, &reason).await;
                        }
                    }
                    // Track task failures
                    EventType::TaskStateFailed => {
                        let should_lower = {
                            let mut tracker = problem_tracker.lock().unwrap();
                            tracker.record_task_failure();
                            tracker.should_lower_mode()
                        };
                        if let Some(reason) = should_lower {
                            lower_mode(&orch_server, &reason).await;
                        }
                    }
                    // Track merge conflicts
                    EventType::MergeConflict => {
                        let should_lower = {
                            let mut tracker = problem_tracker.lock().unwrap();
                            tracker.record_conflict();
                            tracker.should_lower_mode()
                        };
                        if let Some(reason) = should_lower {
                            lower_mode(&orch_server, &reason).await;
                        }
                    }
                    // Skip non-relevant events for merge queue processing
                    _ if !matches!(
                        event.event_type,
                        EventType::MergeQueued
                            | EventType::SystemModePlay
                            | EventType::SystemModePause
                    ) =>
                    {
                        continue;
                    }
                    _ => {}
                }
            }

            // Read current mode — idle in Stop
            let mode = orch_server.mode().await;
            if mode == server::Mode::Stop {
                continue;
            }

            // Snapshot pending merge queue entries with their tasks and projects
            let pending: Vec<(String, String, String)> = {
                let state = orch_server.state.read().await;
                state
                    .merge_queue
                    .pending()
                    .iter()
                    .map(|entry| {
                        (
                            entry.id.clone(),
                            entry.task_id.clone(),
                            entry.pr_url.clone(),
                        )
                    })
                    .collect()
            };

            for (entry_id, task_id, pr_url) in pending {
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
                    Ok(eval) => {
                        // Track successful evaluation
                        if let Ok(mut tracker) = problem_tracker.lock() {
                            tracker.record_eval_success();
                        }
                        eval
                    }
                    Err(e) => {
                        error!(
                            entry_id = %entry_id,
                            task_id = %task_id,
                            error = %e,
                            "orchestrator evaluation failed"
                        );
                        // Track evaluation failure and check for mode lowering
                        let should_lower = {
                            let mut tracker = problem_tracker.lock().unwrap();
                            tracker.record_eval_failure();
                            tracker.should_lower_mode()
                        };
                        if let Some(reason) = should_lower {
                            lower_mode(&orch_server, &reason).await;
                        }
                        continue;
                    }
                };

                // Always emit a decision event (audit trail)
                if let Err(e) = orch_server
                    .emit_orchestrator_decision(
                        &task_id,
                        &entry_id,
                        evaluation.approved,
                        &evaluation.reasoning,
                    )
                    .await
                {
                    error!(error = %e, "failed to emit orchestrator decision event");
                }

                // In Play mode: act on the decision
                if mode == server::Mode::Play {
                    if evaluation.approved {
                        // Track approval (resets rejection counter)
                        if let Ok(mut tracker) = problem_tracker.lock() {
                            tracker.record_approval();
                        }
                        if let Err(e) = orch_server
                            .approve_merge_entry(&entry_id, &evaluation.reasoning)
                            .await
                        {
                            error!(entry_id = %entry_id, error = %e, "failed to approve merge entry");
                        }

                        // Execute the merge on GitHub (Play mode = continuous merge authority)
                        if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(&pr_url) {
                            match merge_github.merge_pull_request(&owner, &repo, number).await {
                                Ok(true) => {
                                    info!(entry_id = %entry_id, pr_url = %pr_url, "PR merged successfully");
                                    if let Err(e) = orch_server.mark_entry_merged(&entry_id, &pr_url).await {
                                        error!(entry_id = %entry_id, error = %e, "failed to mark entry as merged");
                                    }
                                }
                                Ok(false) => {
                                    warn!(entry_id = %entry_id, pr_url = %pr_url, "PR not mergeable (conflicts or checks failing)");
                                    if let Err(e) = orch_server.mark_entry_conflict(&entry_id, &pr_url).await {
                                        error!(entry_id = %entry_id, error = %e, "failed to mark entry as conflict");
                                    }
                                }
                                Err(e) => {
                                    error!(entry_id = %entry_id, pr_url = %pr_url, error = %e, "failed to merge PR on GitHub");
                                }
                            }
                        } else {
                            warn!(entry_id = %entry_id, pr_url = %pr_url, "could not parse PR URL for merge execution");
                        }
                    } else {
                        // Track rejection and check for mode lowering
                        let should_lower = {
                            let mut tracker = problem_tracker.lock().unwrap();
                            tracker.record_rejection();
                            tracker.should_lower_mode()
                        };
                        if let Some(reason) = should_lower {
                            lower_mode(&orch_server, &reason).await;
                        }

                        // Check if the underlying issue is closed (issue #132).
                        // If so, mark the task as completed instead of re-dispatching.
                        let issue_closed = match &task.source {
                            TaskSource::GithubIssue { owner, repo, number } => {
                                match merge_github.get_issue(owner, repo, *number).await {
                                    Ok(issue) => issue.state != IssueState::Open,
                                    Err(e) => {
                                        // On error, assume issue is still open to avoid
                                        // incorrectly marking tasks as complete.
                                        warn!(
                                            task_id = %task_id,
                                            error = %e,
                                            "failed to check issue state, assuming open"
                                        );
                                        false
                                    }
                                }
                            }
                            _ => false,
                        };

                        if issue_closed {
                            // Issue is closed — don't re-dispatch. Mark task as completed.
                            info!(
                                task_id = %task_id,
                                entry_id = %entry_id,
                                "underlying issue is closed, marking task completed instead of re-dispatching"
                            );
                            if let Err(e) = orch_server
                                .reject_merge_entry_closed(
                                    &entry_id,
                                    &format!("{} (issue already closed)", &evaluation.reasoning),
                                    TaskState::Completed,
                                )
                                .await
                            {
                                error!(entry_id = %entry_id, error = %e, "failed to reject merge entry for closed issue");
                            }
                        } else {
                            // Issue is still open — normal rejection with re-dispatch.
                            if let Err(e) = orch_server
                                .reject_merge_entry(
                                    &entry_id,
                                    &evaluation.reasoning,
                                    evaluation.feedback.as_deref(),
                                )
                                .await
                            {
                                error!(entry_id = %entry_id, error = %e, "failed to reject merge entry");
                            }
                            // Emit orchestrator:feedback event when feedback is provided
                            if let Some(feedback) = &evaluation.feedback {
                                if let Err(e) = orch_server.emit_orchestrator_feedback(
                                    &task_id,
                                    feedback,
                                    Some("merge_rejection"),
                                ).await {
                                    error!(error = %e, "failed to emit orchestrator feedback event");
                                }
                            }
                            // TODO: The feedback event is now recorded in the audit trail.
                            // Delivering feedback to the re-dispatched session's prompt
                            // context still needs dispatch loop changes.
                        }
                    }
                }

                // In Pause mode: evaluation is recorded (decision event above)
                // but no merge/reject action is taken. The human reviews.
            }

            // Cleanup terminal merge queue entries (issue #132).
            // This removes Merged and Rejected entries to prevent unbounded growth.
            orch_server.cleanup_merge_queue().await;
        }
    });

    // --- 9. Optionally spawn web server ---

    let web_handle = if config.web {
        // Initialize the completions service for fast LLM utilities
        let completions_service = tasks_agent::CompletionsService::from_env()
            .ok()
            .map(|s| std::sync::Arc::new(tokio::sync::RwLock::new(s)));

        let api_state = crate::web::ApiState {
            server: server.clone(),
            max_sessions: config.max_sessions,
            session_manager: Some(session_manager.clone()),
            completions_service,
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
                let serve = tower_http::services::ServeDir::new(&web_dir).fallback(
                    tower_http::services::ServeFile::new(web_dir.join("index.html")),
                );
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
        crate::tui::run_tui(
            server.clone(),
            server.event_bus.clone(),
            config.max_sessions,
        )
        .await?;
    } else {
        tokio::signal::ctrl_c().await?;
    }
    info!("shutting down");

    // Stop all sessions and destroy their containers
    session_manager.destroy_all().await;

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

/// Per-project workflow settings loaded from `workflow.toml` (spec §14).
struct ProjectWorkflowSettings {
    /// System prompt content from the configured file path.
    system_prompt: Option<String>,
    /// Progress threshold from dispatch config (spec §13.1, §14.2).
    progress_threshold: Option<std::time::Duration>,
}

impl Default for ProjectWorkflowSettings {
    fn default() -> Self {
        Self {
            system_prompt: None,
            progress_threshold: None,
        }
    }
}

/// Load workflow settings for a project at dispatch time (spec §14, §15).
///
/// 1. Fetches `workflow.toml` from the repository root
/// 2. Parses the workflow config
/// 3. Extracts `dispatch.progress_threshold` (spec §13.1, §14.2)
/// 4. Fetches the system prompt file if configured
/// 5. Returns the settings, with defaults for any unavailable fields
///
/// Errors are logged but don't fail dispatch — the session continues with
/// defaults for any fields that couldn't be loaded.
async fn load_workflow_settings_for_project(
    project: Option<&models::project::Project>,
    github_token: &str,
) -> ProjectWorkflowSettings {
    let project = match project {
        Some(p) => p,
        None => return ProjectWorkflowSettings::default(),
    };

    // Parse owner/repo
    let parts: Vec<&str> = project.repo.split('/').collect();
    if parts.len() != 2 {
        warn!(repo = %project.repo, "invalid repo format, cannot load workflow config");
        return ProjectWorkflowSettings::default();
    }
    let (owner, repo) = (parts[0], parts[1]);

    // Create GitHub client
    let client = GitHubClient::new(github_token);

    // Fetch workflow.toml
    let workflow_content = match client.get_file_content(owner, repo, "workflow.toml").await {
        Ok(Some(content)) => content,
        Ok(None) => {
            // No workflow.toml — that's fine, use defaults
            return ProjectWorkflowSettings::default();
        }
        Err(e) => {
            warn!(
                project_id = %project.id,
                error = %e,
                "failed to fetch workflow.toml, using defaults"
            );
            return ProjectWorkflowSettings::default();
        }
    };

    // Parse workflow config
    let workflow_config = match server::workflow::WorkflowConfig::parse(&workflow_content) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(
                project_id = %project.id,
                error = %e,
                "failed to parse workflow.toml, using defaults"
            );
            return ProjectWorkflowSettings::default();
        }
    };

    // Extract progress_threshold from dispatch config (spec §14.2)
    // The workflow.toml default is 60s, so we only override if it differs
    let default_threshold = server::workflow::DispatchConfig::default().progress_threshold;
    let progress_threshold = if workflow_config.dispatch.progress_threshold != default_threshold {
        Some(std::time::Duration::from_secs(
            workflow_config.dispatch.progress_threshold,
        ))
    } else {
        None
    };

    // Fetch system prompt if configured
    let system_prompt = match &workflow_config.prompt.system_prompt {
        Some(system_prompt_path) => {
            match client
                .get_file_content(owner, repo, system_prompt_path)
                .await
            {
                Ok(Some(content)) => {
                    info!(
                        project_id = %project.id,
                        path = %system_prompt_path,
                        "loaded system prompt from workflow config"
                    );
                    Some(content)
                }
                Ok(None) => {
                    warn!(
                        project_id = %project.id,
                        path = %system_prompt_path,
                        "system_prompt file not found in repository"
                    );
                    None
                }
                Err(e) => {
                    warn!(
                        project_id = %project.id,
                        path = %system_prompt_path,
                        error = %e,
                        "failed to fetch system_prompt file"
                    );
                    None
                }
            }
        }
        None => None,
    };

    ProjectWorkflowSettings {
        system_prompt,
        progress_threshold,
    }
}
