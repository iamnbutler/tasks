//! Main run loop — wires all components together.
//!
//! This is intentionally thin — the logic lives in the library crates.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::sync::Mutex as StdMutex;

use tracing::{debug, error, info, warn};

use events::{Actor, Event, EventBus, EventStore, EventType, SYSTEM_TASK_ID};
use uuid::Uuid;
use runtime::{AppleContainerRuntime, ContainerConfig, ContainerRuntime};
use server::Server;
use server::model::merge_queue::{ConflictInfo, ConflictType, MergeQueueEntry};
use server::model::task::{TaskSource, TaskState};
use server::{WorkflowConfigWatcher, RefreshResult, WorkQueue, WorkQueueConfig};
use tokio::sync::RwLock;
use tasks_github::client::GitHubClient;
use tasks_github::model::{IssueState, MergeableState, PullRequestState};
use tasks_github::poller::RepoPoller;

use tasks_orchestrator::{
    ChatContext, ConflictContext, ConflictResolution, OperatingMode, Orchestrator,
    OrchestratorAction, OrchestratorChat, QuestionContext, SystemContext,
};

use crate::config::AppConfig;
use crate::memory::{MemoryGate, MemoryThresholds};
use crate::problem_tracker::ProblemTracker;
use crate::scheduler::AutomationScheduler;
use crate::update::{self, UpdateState};

/// Timeout for a single GitHub poll operation.
///
/// Prevents a slow or unresponsive GitHub API from blocking the entire
/// poll loop. If a poll exceeds this duration, it is cancelled and the
/// next tick will retry. This is a safety net on top of the per-request
/// timeouts configured on the HTTP client.
const POLL_TIMEOUT: Duration = Duration::from_secs(60);

/// Default cooldown duration for rejected PRs (10 minutes).
///
/// After a PR is rejected, the poll loop will skip creating new merge queue
/// entries for the same PR URL + head SHA combination until this duration
/// has elapsed. This prevents the race condition where cleanup removes the
/// Rejected entry and the poll loop immediately re-queues the same PR.
const REJECTED_PR_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Tracks recently rejected PRs to prevent immediate re-queuing (issue #439).
///
/// When the orchestrator rejects a PR, the rejection is recorded here with the
/// PR URL, head SHA, and timestamp. The GitHub poll loop checks this before
/// creating new merge queue entries — if the same PR URL + head SHA was rejected
/// within the cooldown period, the entry is not created.
///
/// This prevents the race condition where:
/// 1. Orchestrator rejects PR, entry becomes Rejected
/// 2. Cleanup removes Rejected entry
/// 3. Poll loop finds open PR, creates new Pending entry
/// 4. Orchestrator re-evaluates (non-deterministically may approve)
struct RejectedPrCooldown {
    /// Map of pr_url -> (head_sha, rejection_time)
    entries: HashMap<String, (String, Instant)>,
}

impl RejectedPrCooldown {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Record a rejection for the given PR URL and head SHA.
    fn record(&mut self, pr_url: String, head_sha: String) {
        self.entries.insert(pr_url, (head_sha, Instant::now()));
    }

    /// Check if creating a new entry should be blocked due to recent rejection.
    ///
    /// Returns true if the PR was rejected within the cooldown period AND the
    /// head SHA matches (i.e., no new commits since rejection).
    fn should_block(&self, pr_url: &str, head_sha: &str, cooldown: Duration) -> bool {
        if let Some((rejected_sha, rejected_at)) = self.entries.get(pr_url) {
            // Block if same SHA and within cooldown period
            rejected_sha == head_sha && rejected_at.elapsed() < cooldown
        } else {
            false
        }
    }

    /// Remove stale entries older than the cooldown duration.
    fn cleanup(&mut self, cooldown: Duration) {
        self.entries
            .retain(|_, (_, rejected_at)| rejected_at.elapsed() < cooldown);
    }
}

/// Result of running the server — indicates how the process should exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    /// Normal shutdown (Ctrl-C or graceful stop).
    Normal,
    /// Restart required for update (exit code 100).
    UpdateRestart,
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

/// Classify a PR merge status into a ConflictType (spec §7.4).
fn classify_conflict(status: &tasks_github::model::PrMergeStatus) -> ConflictType {
    match status.mergeable {
        MergeableState::Unknown => ConflictType::Unknown,
        MergeableState::Mergeable => {
            // Shouldn't happen — merge_pull_request returns Ok(true) for mergeable PRs
            ConflictType::Unknown
        }
        MergeableState::Conflicting => {
            // Check if it's just behind base (needs rebase)
            if status.behind_base_branch && status.conflicting_files.is_empty() {
                return ConflictType::NeedsRebase;
            }

            // Check if conflicts are only in generated/lock files
            if status.is_trivial_conflict() {
                return ConflictType::TrivialMerge;
            }

            // Check if it's a complex conflict
            if status.is_complex_conflict() {
                return ConflictType::ComplexConflict;
            }

            // Default to source conflict
            ConflictType::SourceConflict
        }
    }
}

/// Run the Tasks platform.
///
/// Constructs all components and starts the GitHub poll loop,
/// dispatch tick loop, and session management.
pub async fn run(config: AppConfig) -> Result<RunResult, Box<dyn std::error::Error>> {
    info!(
        data_dir = %config.data_dir,
        max_sessions = config.max_sessions,
        max_sessions_per_project = config.max_sessions_per_project,
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
    let store = Arc::new(tasks_store::Store::open(&db_path)?);

    let event_dir = format!("{}/events", config.data_dir);
    std::fs::create_dir_all(&event_dir)?;
    let event_store = EventStore::new(&event_dir);
    event_store.check_version()?;
    let bus = EventBus::new(event_store, 1024);

    // --- 2. Create server ---

    let server = Arc::new(Server::with_store(bus, store.clone()));
    server
        .load_from_store()
        .await
        .map_err(|e| format!("Failed to load state: {e}"))?;

    // --- 2c. Create work queue ---
    //
    // The centralized work queue replaces the racey dispatch evaluation.
    // All dispatchable work (tasks, PR feedback, conflicts) flows through here.
    let work_queue_config = WorkQueueConfig {
        work_queue_timeout: Duration::from_secs(
            std::env::var("WORK_QUEUE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(15),
        ),
        container_timeout: Duration::from_secs(
            std::env::var("CONTAINER_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2 * 60 * 60),
        ),
        health_check_interval: Duration::from_secs(
            std::env::var("HEALTH_CHECK_INTERVAL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        ),
    };
    let health_check_interval = work_queue_config.health_check_interval;
    let work_queue = Arc::new(RwLock::new(WorkQueue::new(store.clone(), work_queue_config)));

    // --- 2d. Recover orphaned work claims from previous run ---
    //
    // Since we just started, no containers exist yet — any active claims are orphaned.
    // Release them so the work can be re-dispatched to new containers.
    {
        info!("checking for orphaned work claims from previous run");
        let conn = store.conn()?;
        let active_claims = tasks_store::work_claims::get_active_claims(&conn)?;
        drop(conn);

        let mut recovered = 0;
        for claim in active_claims {
            // Since we just started, no containers exist yet — release all active claims
            if claim.container_id.is_some() {
                let conn = store.conn()?;
                tasks_store::work_claims::release_claim(
                    &conn,
                    &claim.work_id,
                    Some("server restart - container no longer exists"),
                )?;
                drop(conn);
                recovered += 1;
            }
        }

        if recovered > 0 {
            info!(count = recovered, "released orphaned work claims from previous run");
        }
    }

    // --- 2b. Create workflow config watcher (spec §14.3) ---
    //
    // Watches workflow.toml in project repositories and caches configs.
    // Refreshes are triggered during the GitHub poll loop.
    let workflow_config_watcher = Arc::new(WorkflowConfigWatcher::with_defaults());

    // --- 3. Create container runtime ---

    let container_runtime = AppleContainerRuntime::new();
    container_runtime.health_check().await.map_err(|e| {
        format!("container runtime is not available: {e}")
    })?;

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

    let orchestrator = Arc::new(tasks_orchestrator::ClaudeOrchestrator::from_env().map_err(
        |e| {
            format!(
                "failed to initialize orchestrator: {e}. Set ANTHROPIC_API_KEY in .env to fix."
            )
        },
    )?);
    info!("orchestrator initialized (Claude-backed)");

    // --- 5b2. Create orchestrator chat handler ---

    let orchestrator_chat: Option<Arc<OrchestratorChat>> = match OrchestratorChat::from_env() {
        Ok(chat) => {
            info!("orchestrator chat initialized");
            Some(Arc::new(chat))
        }
        Err(e) => {
            warn!(error = %e, "orchestrator chat not available");
            None
        }
    };
    let chat_history: Arc<tokio::sync::Mutex<Vec<tasks_agent::Message>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

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

    // --- Shutdown broadcast channel ---
    //
    // Each spawned task subscribes to this channel. On shutdown, we send a
    // signal so tasks can finish in-flight work before being aborted.
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    let watchdog_gate = memory_gate.clone();
    let watchdog_bus = server.event_bus.clone();
    let watchdog_sessions = session_manager.clone();
    let mut watchdog_shutdown_rx = shutdown_tx.subscribe();

    let watchdog_handle = tokio::spawn(async move {
        tokio::select! {
            _ = crate::memory::watchdog_loop(
                watchdog_gate,
                memory_thresholds,
                watchdog_bus,
                watchdog_sessions,
                std::time::Duration::from_secs(10),
            ) => {}
            _ = watchdog_shutdown_rx.recv() => {
                info!("watchdog received shutdown signal");
            }
        }
    });

    // --- 5d. Create update checker ---
    //
    // Background task that periodically checks for git updates.
    // When updates are detected, it updates the shared state. If auto_apply
    // is enabled, it will trigger a graceful shutdown with exit code 100.

    let update_state = Arc::new(UpdateState::new());
    let update_handle = if config.update_check_enabled {
        let state = update_state.clone();
        let repo_path = std::env::current_dir().unwrap_or_default();
        let check_interval = config.update_check_interval;
        let mut update_shutdown_rx = shutdown_tx.subscribe();

        info!(
            interval = ?check_interval,
            auto_apply = config.update_auto_apply,
            "update checker enabled"
        );

        Some(tokio::spawn(async move {
            tokio::select! {
                _ = update::update_checker_loop(state, repo_path, check_interval) => {}
                _ = update_shutdown_rx.recv() => {
                    info!("update checker received shutdown signal");
                }
            }
        }))
    } else {
        info!("update checker disabled");
        None
    };

    // --- 6. Emit system:started ---

    let project_count = server.state.read().await.projects.len();
    server.emit_started().await?;
    info!(projects = project_count, "tasks platform started");

    // --- 6b. Spawn automation scheduler ---
    //
    // The scheduler evaluates cron expressions for scheduled automations
    // and triggers runs when their schedules match. It ticks every 60 seconds
    // and respects operating mode (no runs in Stop mode).

    let automation_scheduler = AutomationScheduler::new(
        server.clone(),
        Some(session_manager.clone()),
        config.automation_soft_limit,
        config.automation_hard_limit,
    );
    let automation_scheduler_shutdown_rx = shutdown_tx.subscribe();
    let automation_scheduler_handle = automation_scheduler.start(automation_scheduler_shutdown_rx);
    info!("automation scheduler started");

    // --- 6c. Spawn automation event listener ---
    //
    // Watches for session completion/failure events on automation sessions
    // (task_id starting with "automation-run:") and updates run records.

    let automation_listener_shutdown_rx = shutdown_tx.subscribe();
    let automation_listener_handle = crate::automation_runner::spawn_automation_event_listener(
        &server.event_bus,
        server.clone(),
        automation_listener_shutdown_rx,
    );
    info!("automation event listener started");

    // --- 6d. Create rejected PR cooldown tracker (issue #439) ---
    //
    // Shared between the poll loop and orchestrator loop to prevent re-queuing
    // PRs that were recently rejected with the same head SHA. The orchestrator
    // records rejections, and the poll loop checks before creating new entries.
    let rejected_pr_cooldown = Arc::new(StdMutex::new(RejectedPrCooldown::new()));

    // --- 7. Spawn GitHub poll loop ---
    //
    // Pollers are rebuilt from the live project list each tick so that
    // projects added/removed via the web UI take effect immediately.
    // Also refreshes workflow configs (spec §14.3) on each tick.

    let poll_server = server.clone();
    let poll_interval = config.poll_interval;
    let github_token = config.github_token.clone();
    let poll_config_watcher = workflow_config_watcher.clone();
    let poll_rejected_cooldown = rejected_pr_cooldown.clone();
    let mut poll_shutdown_rx = shutdown_tx.subscribe();

    let poll_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(poll_interval);
        let label_config = server::workflow::LabelConfig::default();
        let mut pollers: HashMap<String, RepoPoller> = HashMap::new();

        // Exponential backoff state for GitHub API failures (issue #510).
        // Tracks consecutive failure count and the earliest time each project
        // is eligible for the next poll attempt.
        let mut poll_failures: HashMap<String, u32> = HashMap::new();
        let mut poll_backoff_until: HashMap<String, Instant> = HashMap::new();
        const MAX_BACKOFF_EXPONENT: u32 = 6; // cap at 2^6 * base = 64× base interval

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = poll_shutdown_rx.recv() => {
                    info!("poll loop received shutdown signal");
                    break;
                }
            }

            // Check if rebuild was requested (issue #256)
            // If so, clear all pollers so they're recreated with since=None
            if poll_server.take_rebuild_requested() {
                info!("rebuild requested, clearing pollers for full re-fetch");
                pollers.clear();
            }

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
                        // Load persisted high-water mark to avoid cold start (spec github.md §5.3)
                        let since = match poll_server.get_last_polled_at(project_id) {
                            Ok(ts) => {
                                if ts.is_some() {
                                    debug!(
                                        project = %project_id,
                                        since = ?ts,
                                        "restored poller high-water mark from database"
                                    );
                                }
                                ts
                            }
                            Err(e) => {
                                warn!(
                                    project = %project_id,
                                    error = %e,
                                    "failed to load poller high-water mark, starting cold"
                                );
                                None
                            }
                        };
                        let poller = RepoPoller::new(client, parts[0], parts[1]).with_since(since);
                        pollers.insert(project_id.clone(), poller);
                    }
                }
            }

            // Refresh workflow configs (spec §14.3)
            //
            // Check for workflow.toml changes in each project repository.
            // On change, emit system:config:reloaded event.
            for (project_id, repo) in &projects {
                let parts: Vec<&str> = repo.split('/').collect();
                if parts.len() != 2 {
                    continue;
                }
                let (owner, repo_name) = (parts[0].to_string(), parts[1].to_string());
                let token = github_token.clone();
                let pid = project_id.clone();

                let result = match tokio::time::timeout(
                    POLL_TIMEOUT,
                    poll_config_watcher.refresh(&pid, || async {
                        let client = GitHubClient::new(&token);
                        client
                            .get_file_content(&owner, &repo_name, "workflow.toml")
                            .await
                            .map_err(|e| e.to_string())
                    }),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_elapsed) => {
                        warn!(project = %pid, "workflow config refresh timed out");
                        continue;
                    }
                };

                // Emit event if config changed
                if let RefreshResult::Changed(config) = result {
                    info!(
                        project_id = %pid,
                        max_sessions = ?config.project.max_sessions,
                        max_retries = config.dispatch.max_retries,
                        "workflow config reloaded"
                    );
                    let event = Event::new(
                        EventType::SystemConfigReloaded,
                        "system",
                        Actor::System,
                        serde_json::json!({
                            "project_id": pid,
                            "config": {
                                "max_sessions": config.project.max_sessions,
                                "max_retries": config.dispatch.max_retries,
                                "retry_base_delay": config.dispatch.retry_base_delay,
                                "progress_threshold": config.dispatch.progress_threshold,
                                "ignore_labels": config.labels.ignore,
                                "blocked_labels": config.labels.blocked,
                            }
                        }),
                    );
                    if let Err(e) = poll_server.event_bus.publish(event).await {
                        error!(error = %e, "failed to publish config reload event");
                    }
                }
            }

            for (project_id, poller) in pollers.iter_mut() {
                // Skip projects in backoff (issue #510)
                if let Some(&until) = poll_backoff_until.get(project_id.as_str()) {
                    if Instant::now() < until {
                        debug!(
                            project = %project_id,
                            backoff_remaining_secs = until.duration_since(Instant::now()).as_secs(),
                            "skipping poll, in backoff after consecutive failures"
                        );
                        continue;
                    }
                }

                // Wrap poll in timeout to prevent stalls (issue #574)
                let poll_result = tokio::time::timeout(POLL_TIMEOUT, poller.poll()).await;
                let poll_result = match poll_result {
                    Ok(inner) => inner,
                    Err(_elapsed) => {
                        error!(project = %project_id, "poll timed out after {}s", POLL_TIMEOUT.as_secs());
                        continue;
                    }
                };
                match poll_result {
                    Ok(result) => {
                        // Reset backoff on success (issue #510)
                        if poll_failures.remove(project_id.as_str()).is_some() {
                            poll_backoff_until.remove(project_id.as_str());
                            info!(project = %project_id, "poll succeeded, backoff reset");
                        }
                        // --- Issue processing: create or reconcile (issue #254) ---
                        for issue in &result.issues {
                            let source = TaskSource::GithubIssue {
                                owner: issue.owner.clone(),
                                repo: issue.repo.clone(),
                                number: issue.number,
                            };

                            if let Some(task_id) = poll_server.task_id_for_source(&source).await {
                                // Task exists — reconcile with fresh GitHub data
                                match poll_server.reconcile_task(&task_id, issue, &label_config).await {
                                    Ok(Some(result)) if result.has_changes() => {
                                        if let Some(new_state) = result.new_state {
                                            info!(
                                                project = %project_id,
                                                issue = issue.number,
                                                task_id = %task_id,
                                                new_state = ?new_state,
                                                updated_fields = ?result.updated_fields,
                                                "reconciled task from GitHub"
                                            );
                                        } else if !result.updated_fields.is_empty() {
                                            debug!(
                                                project = %project_id,
                                                issue = issue.number,
                                                task_id = %task_id,
                                                updated_fields = ?result.updated_fields,
                                                "synced task metadata from GitHub"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            project = %project_id,
                                            issue = issue.number,
                                            task_id = %task_id,
                                            error = %e,
                                            "failed to reconcile task"
                                        );
                                    }
                                    _ => {} // No changes or task not found
                                }
                            } else {
                                // New issue — create task (existing behavior)
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

                        // --- PR processing: create merge entries + reconcile (issue #255) ---
                        //
                        // Add open, non-draft PRs to the merge queue. We don't create
                        // tasks for PRs — that would cause loops where agent-created PRs
                        // get picked up as new tasks. The merge queue is PR-centric.
                        for pr in &result.pull_requests {
                            // Only create new entries for open, non-draft PRs
                            if pr.state == tasks_github::model::PullRequestState::Open && !pr.is_draft {
                                let pr_url = format!(
                                    "https://github.com/{}/{}/pull/{}",
                                    pr.owner, pr.repo, pr.number
                                );

                                // Check cooldown for recently rejected PRs (issue #439).
                                // If this PR was rejected with the same head SHA within the cooldown
                                // period, skip creating a new entry to prevent immediate re-queuing.
                                let blocked_by_cooldown = {
                                    let cooldown = poll_rejected_cooldown.lock().unwrap();
                                    cooldown.should_block(&pr_url, &pr.head_sha, REJECTED_PR_COOLDOWN)
                                };
                                if blocked_by_cooldown {
                                    debug!(
                                        project = %project_id,
                                        pr = pr.number,
                                        head_sha = %pr.head_sha,
                                        "skipping PR re-queue: recently rejected with same SHA"
                                    );
                                    continue;
                                }

                                if !poll_server.has_merge_entry_for_pr(&pr_url).await {
                                    // Priority 1: Branch name match (existing, fast, precise)
                                    let mut task_id = poll_server
                                        .find_task_by_branch(&pr.head_ref)
                                        .await;

                                    // Priority 2: PR's linked_issues (GitHub's own linkage via
                                    // closing keywords or manual links) — issue #258
                                    if task_id.is_none() {
                                        for linked_issue in &pr.linked_issues {
                                            if let Some(id) = poll_server
                                                .find_task_by_github_issue(
                                                    &pr.owner,
                                                    &pr.repo,
                                                    linked_issue.number,
                                                )
                                                .await
                                            {
                                                debug!(
                                                    pr = pr.number,
                                                    issue = linked_issue.number,
                                                    task_id = %id,
                                                    "linked task via PR's linked_issues"
                                                );
                                                task_id = Some(id);
                                                break;
                                            }
                                        }
                                    }

                                    let task_id = task_id.unwrap_or_default();
                                    let entry_id = format!("mq-{}-{}-pr-{}", pr.owner, pr.repo, pr.number);
                                    let entry = MergeQueueEntry::new(
                                        entry_id.clone(),
                                        task_id.clone(),
                                        &pr_url,
                                    )
                                    .with_head_sha(&pr.head_sha);

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
                        }

                        // Reconcile merge queue: detect externally merged/closed PRs,
                        // update conflict status from GitHub's mergeable field.
                        match poll_server.reconcile_merge_queue(&result.pull_requests).await {
                            Ok(changes) if changes > 0 => {
                                info!(
                                    project = %project_id,
                                    changes = changes,
                                    "reconciled merge queue from GitHub PR data"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    project = %project_id,
                                    error = %e,
                                    "failed to reconcile merge queue"
                                );
                            }
                            _ => {} // No changes
                        }

                        // --- Retry approved merges (issue #467) ---
                        //
                        // In Play mode, pick up any Approved entries that haven't
                        // transitioned to Merging/Merged yet. This handles the case
                        // where a merge was reverted to Approved after a transient
                        // GitHub API failure.
                        if poll_server.mode().await == server::Mode::Play {
                            let approved_entries: Vec<(String, String)> = {
                                let state = poll_server.state.read().await;
                                state.merge_queue.approved().iter().map(|e| {
                                    (e.id.clone(), e.pr_url.clone())
                                }).collect()
                            };

                            if !approved_entries.is_empty() {
                                let retry_token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
                                if !retry_token.is_empty() {
                                    let retry_client = GitHubClient::new(&retry_token);
                                    for (entry_id, pr_url) in &approved_entries {
                                        if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(pr_url) {
                                            info!(entry_id = %entry_id, pr_url = %pr_url, "retrying merge for approved entry");
                                            if let Err(e) = poll_server.mark_entry_merging(entry_id, pr_url).await {
                                                error!(entry_id = %entry_id, error = %e, "failed to mark entry as merging for retry");
                                                continue;
                                            }
                                            match retry_client.merge_pull_request(&owner, &repo, number).await {
                                                Ok(true) => {
                                                    info!(entry_id = %entry_id, pr_url = %pr_url, "PR merged on retry");
                                                    if let Err(e) = poll_server.mark_entry_merged(entry_id, pr_url).await {
                                                        error!(entry_id = %entry_id, error = %e, "failed to mark entry merged on retry");
                                                    }
                                                }
                                                Ok(false) => {
                                                    warn!(entry_id = %entry_id, pr_url = %pr_url, "PR not mergeable on retry");
                                                    if let Err(e) = poll_server.mark_entry_conflict(entry_id, pr_url, None).await {
                                                        error!(entry_id = %entry_id, error = %e, "failed to mark entry conflict on retry");
                                                    }
                                                }
                                                Err(e) => {
                                                    error!(entry_id = %entry_id, pr_url = %pr_url, error = %e, "merge retry failed, will retry next poll cycle");
                                                    if let Err(e) = poll_server.revert_entry_to_approved(entry_id, pr_url).await {
                                                        error!(entry_id = %entry_id, error = %e, "failed to revert entry to Approved on retry failure");
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // --- PR closure: transition linked tasks (spec §11.3) ---
                        //
                        // When a PR is merged/closed externally, transition the linked
                        // task. This is separate from merge queue reconciliation because
                        // the task linkage is via branch name, not merge queue entry.
                        for pr in &result.pull_requests {
                            if pr.state == PullRequestState::Open {
                                continue;
                            }

                            // Priority 1: Branch name match
                            let mut task_id = poll_server.find_task_by_branch(&pr.head_ref).await;

                            // Priority 2: PR's linked_issues (issue #258)
                            if task_id.is_none() {
                                for linked_issue in &pr.linked_issues {
                                    if let Some(id) = poll_server
                                        .find_task_by_github_issue(
                                            &pr.owner,
                                            &pr.repo,
                                            linked_issue.number,
                                        )
                                        .await
                                    {
                                        debug!(
                                            pr = pr.number,
                                            issue = linked_issue.number,
                                            task_id = %id,
                                            "linked task via PR's linked_issues for closure"
                                        );
                                        task_id = Some(id);
                                        break;
                                    }
                                }
                            }

                            let task_id = match task_id {
                                Some(id) => id,
                                None => continue,
                            };

                            let task = match poll_server.get_task(&task_id).await {
                                Some(t) => t,
                                None => continue,
                            };

                            if task.state.is_terminal() {
                                continue;
                            }

                            match pr.state {
                                PullRequestState::Merged => {
                                    info!(
                                        project = %project_id,
                                        pr = pr.number,
                                        task_id = %task_id,
                                        "detected external PR merge, transitioning task to Completed"
                                    );
                                    if let Err(e) = poll_server
                                        .set_task_state_with_data(
                                            &task_id,
                                            TaskState::Completed,
                                            Actor::Scheduler,
                                            serde_json::json!({ "source": "reconciliation" }),
                                        )
                                        .await
                                    {
                                        warn!(task_id = %task_id, error = %e, "failed to transition task for merged PR");
                                    }
                                }
                                PullRequestState::Closed => {
                                    if task.state == TaskState::AwaitingMerge {
                                        info!(
                                            project = %project_id,
                                            pr = pr.number,
                                            task_id = %task_id,
                                            "detected external PR closure, transitioning task to Cancelled"
                                        );
                                        if let Err(e) = poll_server
                                            .set_task_state_with_data(
                                                &task_id,
                                                TaskState::Cancelled,
                                                Actor::Scheduler,
                                                serde_json::json!({ "source": "reconciliation" }),
                                            )
                                            .await
                                        {
                                            warn!(task_id = %task_id, error = %e, "failed to transition task for closed PR");
                                        }
                                    } else {
                                        debug!(
                                            project = %project_id,
                                            pr = pr.number,
                                            task_id = %task_id,
                                            task_state = ?task.state,
                                            "PR closed but task not in AwaitingMerge, skipping (likely rework)"
                                        );
                                    }
                                }
                                PullRequestState::Open => {
                                    // Filtered out at loop start; log if we somehow get here
                                    debug!(pr = pr.number, "skipping Open PR in closure handler");
                                }
                            }
                        }

                        // Persist the high-water mark after successful poll (spec github.md §5.3)
                        if let Some(ts) = result.timestamp {
                            if let Err(e) = poll_server.set_last_polled_at(project_id, ts) {
                                warn!(
                                    project = %project_id,
                                    error = %e,
                                    "failed to persist poller high-water mark"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let failures = poll_failures
                            .entry(project_id.clone())
                            .and_modify(|c| *c = c.saturating_add(1))
                            .or_insert(1);
                        let exponent = (*failures).min(MAX_BACKOFF_EXPONENT);
                        let backoff = poll_interval * 2u32.pow(exponent);
                        poll_backoff_until
                            .insert(project_id.clone(), Instant::now() + backoff);
                        error!(
                            project = %project_id,
                            error = %e,
                            consecutive_failures = *failures,
                            backoff_secs = backoff.as_secs(),
                            "poll failed, backing off"
                        );
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
    let event_handler_work_queue = work_queue.clone();
    let event_handler_max_retries = config.max_retries;
    let mut event_handler_shutdown_rx = shutdown_tx.subscribe();

    let event_handler_handle = tokio::spawn(async move {
        let mut rx = event_handler_bus.subscribe();
        loop {
            let event = tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(e) => e,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            error!(
                                skipped = n,
                                "event handler lagged — {n} events dropped from broadcast channel. \
                                 Replaying recent state events from store to recover."
                            );
                            // Recovery: replay the latest state event for each task from
                            // the persistent event store so we don't permanently lose
                            // state transitions. Events are always persisted before
                            // broadcast, so the store is the source of truth.
                            match event_handler_bus.query_by_type_prefix("task:state:", 500).await {
                                Ok(state_events) => {
                                    // Collect the latest state event per task
                                    let mut latest_per_task: std::collections::HashMap<String, events::Event> =
                                        std::collections::HashMap::new();
                                    for ev in state_events {
                                        latest_per_task
                                            .entry(ev.task.clone())
                                            .and_modify(|existing| {
                                                if ev.ts > existing.ts {
                                                    *existing = ev.clone();
                                                }
                                            })
                                            .or_insert(ev);
                                    }
                                    let replayed = latest_per_task.len();
                                    for (_task_id, ev) in &latest_per_task {
                                        if ev.actor == events::Actor::Scheduler {
                                            continue;
                                        }
                                        let state = match ev.event_type {
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
                                        if let Some(s) = state {
                                            if let Err(e) = event_handler_server.apply_task_state(&ev.task, s).await {
                                                if !matches!(e, server::ServerError::TaskNotFound(_)) {
                                                    error!(task_id = %ev.task, error = %e, "failed to replay task state during lag recovery");
                                                }
                                            }
                                        }
                                    }
                                    info!(
                                        tasks_replayed = replayed,
                                        "lag recovery complete — replayed latest state for {replayed} tasks"
                                    );
                                }
                                Err(e) => {
                                    error!(
                                        error = %e,
                                        "lag recovery failed — could not read state events from store"
                                    );
                                }
                            }
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                _ = event_handler_shutdown_rx.recv() => {
                    info!("event handler received shutdown signal");
                    break;
                }
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

                // Extract failure_info from event data for diagnosis (spec §13.4)
                let failure_info = event.data.get("failure_info")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());

                if let Err(e) = event_handler_server
                    .handle_task_failure(task_id, made_progress, event_handler_max_retries, failure_info)
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

                // Complete the work item when task reaches terminal state
                if state.is_terminal() {
                    let work_id = format!("task:{}", task_id);
                    let mut queue = event_handler_work_queue.write().await;
                    if let Err(e) = queue.complete(&work_id) {
                        warn!(work_id = %work_id, error = %e, "failed to complete work item");
                    }

                    // Clear the session_id
                    if let Err(e) = event_handler_server.clear_task_session_id(task_id).await {
                        warn!(task_id = %task_id, error = %e, "failed to clear session_id");
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
    let dispatch_work_queue = work_queue.clone();
    let dispatch_interval = config.dispatch_interval;
    let max_sessions = config.max_sessions;
    let _max_sessions_per_project = config.max_sessions_per_project; // TODO: wire into work queue per-project limits
    let max_retries = config.max_retries;
    let dispatch_event_bus = server.event_bus.clone();
    let dispatch_memory_gate = memory_gate.clone();
    let dispatch_github_token = config.github_token.clone();
    let dispatch_config_watcher = workflow_config_watcher.clone();
    let mut dispatch_shutdown_rx = shutdown_tx.subscribe();

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
            // Wait for either the tick, a dispatch-triggering event, or shutdown
            let trigger_event = tokio::select! {
                _ = interval.tick() => None,
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => Some(event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "dispatch loop lagged — {n} events dropped, next reconciliation tick will catch up");
                            None
                        }
                        Err(_) => continue, // channel closed, will be handled by outer loop
                    }
                }
                _ = dispatch_shutdown_rx.recv() => {
                    info!("dispatch loop received shutdown signal");
                    break;
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

            // Check operating mode — no dispatch in Stop mode (spec §6.1)
            let mode = dispatch_server.mode().await;
            if !mode.allows_dispatch() {
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

            // --- Handle pending answers (resume Question-state tasks) ---
            // Process pending answers first — these are tasks that were in Question
            // state and received a HumanMessage. The message was already sent to
            // the session, so we just need to track that the answer was processed.
            let answers_vec: Vec<String> = pending_answers.iter().cloned().collect();
            if !answers_vec.is_empty() {
                // Use the old dispatcher for resume logic (pending answers)
                // This is separate from new work dispatch and doesn't have the race condition
                match dispatch_server.run_dispatch_with_limits(&answers_vec, 0, None).await {
                    Ok(plan) => {
                        for task_id in &plan.resume {
                            info!(task_id = %task_id, "resumed session with pending answer");
                            pending_answers.remove(task_id);
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "dispatch (resume) failed");
                    }
                }
            }

            // --- Rebuild work queue from current state ---
            {
                let state = dispatch_server.state.read().await;
                let mut queue = dispatch_work_queue.write().await;
                if let Err(e) = queue.rebuild(&state.tasks) {
                    error!(error = %e, "failed to rebuild work queue");
                    continue;
                }
            }

            // --- Claim next work item ---
            let active_count = dispatch_session_mgr.active_count().await;
            let container_id = Uuid::new_v4().to_string();

            let claimed = {
                let mut queue = dispatch_work_queue.write().await;
                queue.claim_next(effective_max as usize, active_count, &container_id)
            };

            let work_item = match claimed {
                Ok(Some(item)) => item,
                Ok(None) => continue, // No work available or rate limited
                Err(e) => {
                    error!(error = %e, "failed to claim work");
                    continue;
                }
            };

            // --- Start session for the claimed work ---
            let task_id = work_item.source_id.clone();
            let Some(task) = dispatch_server.get_task(&task_id).await else {
                warn!(task_id = %task_id, "claimed work item has no corresponding task");
                // Release the claim since we can't process it
                let mut queue = dispatch_work_queue.write().await;
                if let Err(e) = queue.release(&work_item.id, Some("task not found")) {
                    warn!(work_id = %work_item.id, error = %e, "failed to release claim");
                }
                continue;
            };

            let project = dispatch_server.get_project(&task.project).await;
            let repo_url = project
                .as_ref()
                .map(|p| format!("https://github.com/{}.git", p.repo))
                .unwrap_or_default();

            // Include unique suffix to prevent branch name clashes on task
            // retry and to prevent agents from rediscovering past attempts.
            let unique_suffix = &Uuid::new_v4().to_string()[..8];
            let branch = format!("tasks/{}--{}", task.id, unique_suffix);

            // Load workflow settings (spec §14, §15)
            // Uses cached config from the workflow watcher (spec §14.3)
            let workflow_settings = load_workflow_settings_for_project(
                project.as_ref(),
                &dispatch_github_token,
                &dispatch_config_watcher,
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

            // Transition task to Running before starting session
            if let Err(e) = dispatch_server
                .set_task_state(&task_id, TaskState::Running, Actor::Scheduler)
                .await
            {
                error!(task_id = %task_id, error = %e, "failed to transition task to Running");
                // Release the claim
                let mut queue = dispatch_work_queue.write().await;
                if let Err(e2) = queue.release(&work_item.id, Some(&format!("state transition failed: {}", e))) {
                    warn!(work_id = %work_item.id, error = %e2, "failed to release claim");
                }
                continue;
            }

            match dispatch_session_mgr
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
                Ok(_) => {
                    info!(task_id = %task_id, work_id = %work_item.id, "session started for claimed work");
                    // Clear rejection feedback after successful dispatch (issue #423).
                    // This prevents stale feedback from being repeated if the task
                    // is rejected and re-dispatched again.
                    if task.rejection_feedback.is_some() {
                        if let Err(e) = dispatch_server.clear_task_rejection_feedback(&task_id).await {
                            warn!(task_id = %task_id, error = %e, "failed to clear rejection feedback after dispatch");
                        }
                    }
                }
                Err(e) => {
                    error!(task_id = %task_id, error = %e, "failed to start session");

                    // Release the claim
                    let mut queue = dispatch_work_queue.write().await;
                    if let Err(e2) = queue.release(&work_item.id, Some(&format!("session start failed: {}", e))) {
                        warn!(work_id = %work_item.id, error = %e2, "failed to release claim");
                    }

                    // Handle failure for backoff — treat as no progress so backoff kicks in.
                    // This prevents tight retry loops when containers can't start.
                    if let Err(e2) = dispatch_server
                        .handle_task_failure(&task_id, false, max_retries, None)
                        .await
                    {
                        warn!(task_id = %task_id, error = %e2, "failed to handle session start failure");
                    }
                }
            }
        }
    });

    // --- 8b. Spawn orchestrator loop ---
    //
    // The orchestrator is the project foreman. On each tick it observes
    // project state and acts: evaluates merge queue entries, comments on
    // PRs, adjusts tasks. Currently only merge queue evaluation is wired.
    //
    // Mode behavior (issue #337):
    // - Play mode: fully autonomous — approves+merges, rejects+re-dispatches,
    //   handles conflicts.
    // - Pause mode: evaluates and acts on rejections, conflicts, and
    //   changes-requested normally, but holds approved merges for human
    //   flush. Only the actual merge-on-approval is paused.
    // - Stop mode: idle, no evaluation or action.
    //
    // Mode lowering (spec §6.4): The orchestrator tracks problem patterns
    // and can lower mode from Play to Pause when things go wrong.

    let orch_server = server.clone();
    let orch = orchestrator.clone();
    let think_orch = orchestrator.clone();
    let orch_event_bus = server.event_bus.clone();
    let orch_github_token = config.github_token.clone();
    let orch_rejected_cooldown = rejected_pr_cooldown.clone();
    let orch_session_mgr = session_manager.clone();
    let mut orch_shutdown_rx = shutdown_tx.subscribe();

    let orchestrator_eval_interval = config.orchestrator_eval_interval;
    let conflict_max_age = config.conflict_max_age;

    // Problem tracker for mode lowering (spec §6.4)
    let problem_tracker = Arc::new(StdMutex::new(ProblemTracker::new()));

    /// Check if a merge queue entry needs evaluation: never evaluated, or has
    /// new commits (head_sha changed since last evaluation).
    fn needs_eval(
        evaluated_prs: &std::collections::HashMap<String, String>,
        entry: &server::model::merge_queue::MergeQueueEntry,
    ) -> bool {
        match evaluated_prs.get(&entry.pr_url) {
            None => true,
            Some(last_sha) => entry.head_sha.as_ref().is_some_and(|sha| sha != last_sha),
        }
    }

    let orchestrator_handle = tokio::spawn(async move {
        let mut eval_interval = tokio::time::interval(orchestrator_eval_interval);
        let mut event_rx = orch_event_bus.subscribe();
        let merge_github = GitHubClient::new(&orch_github_token);

        // FIFO queue of entry IDs waiting for evaluation.
        // Entries are pushed when MergeQueued events arrive and popped one at a time
        // on each eval_interval tick. This throttles LLM calls to at most one per
        // interval (default 15s) while human chat messages bypass the queue entirely.
        let mut eval_queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        // Track which entry IDs are already in the queue to avoid duplicates.
        let mut eval_queued: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Track the PR URL (used as a proxy for "which PR") that was last evaluated for
        // each entry, so we don't re-evaluate the same PR until it has new commits.
        // Key: pr_url, Value: last evaluated head SHA.
        let mut evaluated_prs: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        loop {
            // Either the eval timer fires (pop one entry) or an event arrives
            let event_opt = tokio::select! {
                _ = eval_interval.tick() => None,
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => Some(event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "orchestrator loop lagged — {n} events dropped, next eval tick will catch up");
                            continue;
                        }
                        Err(_) => break, // channel closed, shut down
                    }
                }
                _ = orch_shutdown_rx.recv() => {
                    info!("orchestrator loop received shutdown signal");
                    break;
                }
            };

            // Handle events: problem tracking, mode changes, chat, and queue additions
            if let Some(ref event) = event_opt {
                match event.event_type {
                    // When a new entry is queued, add it to the FIFO
                    EventType::MergeQueued => {
                        if let Some(entry_id) = event.data.get("entry_id").and_then(|v| v.as_str()) {
                            // Look up entry to check if it actually needs evaluation
                            let skip = {
                                let state = orch_server.state.read().await;
                                state.merge_queue.get(entry_id)
                                    .map(|e| !needs_eval(&evaluated_prs, e))
                                    .unwrap_or(false)
                            };
                            if !skip && eval_queued.insert(entry_id.to_string()) {
                                eval_queue.push_back(entry_id.to_string());
                                info!(entry_id = %entry_id, queue_len = eval_queue.len(), "added entry to eval queue");
                            }
                        } else {
                            // Fallback: scan for any pending entries not yet queued/evaluated
                            let state = orch_server.state.read().await;
                            for entry in state.merge_queue.pending() {
                                if needs_eval(&evaluated_prs, &entry) && eval_queued.insert(entry.id.clone()) {
                                    eval_queue.push_back(entry.id.clone());
                                }
                            }
                        }
                        continue;
                    }
                    // Reset problem tracker when human raises mode to Play
                    EventType::SystemModePlay => {
                        if let Ok(mut tracker) = problem_tracker.lock() {
                            tracker.reset();
                            info!("problem tracker reset (mode raised to Play)");
                        }
                        // Seed the queue with pending entries not yet evaluated
                        let state = orch_server.state.read().await;
                        for entry in state.merge_queue.pending() {
                            // Check if PR needs evaluation: never evaluated or has new commits
                            let needs_eval = match evaluated_prs.get(&entry.pr_url) {
                                None => true,
                                Some(last_sha) => entry.head_sha.as_ref().is_some_and(|sha| sha != last_sha),
                            };
                            if needs_eval && eval_queued.insert(entry.id.clone()) {
                                eval_queue.push_back(entry.id.clone());
                            }
                        }
                    }
                    // Pause mode now actively evaluates and processes rejections/
                    // conflicts (issue #337), so seed the queue on mode change.
                    EventType::SystemModePause => {
                        let state = orch_server.state.read().await;
                        for entry in state.merge_queue.pending() {
                            if !evaluated_prs.contains_key(&entry.pr_url) && eval_queued.insert(entry.id.clone()) {
                                eval_queue.push_back(entry.id.clone());
                            }
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
                    // Handle stuck agents: when a task enters Question state, the
                    // orchestrator answers the question to unblock the agent (issue #533).
                    EventType::TaskStateQuestion => {
                        let task_id = event.task.clone();
                        let human_present = orch_server.is_human_present();

                        // Extract the question text from event data, or look up
                        // the most recent agent:question event for this task.
                        let question = event.data.get("question")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                event.data.get("message")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            });

                        // If the question text isn't in the event data, try to
                        // find it from recent events for this task.
                        let question = match question {
                            Some(q) if !q.is_empty() => q,
                            _ => {
                                match orch_server.event_bus.read_task(&task_id).await {
                                    Ok(events) => {
                                        events.iter().rev()
                                            .find(|e| e.event_type == EventType::AgentQuestion)
                                            .and_then(|e| {
                                                e.data.get("question")
                                                    .or_else(|| e.data.get("message"))
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string())
                                            })
                                            .unwrap_or_else(|| "(no question text found)".to_string())
                                    }
                                    Err(e) => {
                                        warn!(task_id = %task_id, error = %e, "failed to read task events for question text");
                                        "(failed to retrieve question)".to_string()
                                    }
                                }
                            }
                        };

                        // If human is present, surface the question as an escalation
                        // instead of answering autonomously.
                        if human_present {
                            info!(
                                task_id = %task_id,
                                "agent stuck with question, human present — surfacing as escalation"
                            );
                            let escalation_event = Event::new(
                                EventType::OrchestratorEscalation,
                                &task_id,
                                Actor::Orchestrator,
                                serde_json::json!({
                                    "action": "agent_question",
                                    "message": format!("Agent is stuck and asking: {}", question),
                                }),
                            );
                            if let Err(e) = orch_server.event_bus.publish(escalation_event).await {
                                error!(error = %e, "failed to emit escalation for agent question");
                            }
                            continue;
                        }

                        // No human present — orchestrator answers autonomously.
                        // Look up the task and project for context.
                        let (task, project) = {
                            let state = orch_server.state.read().await;
                            let task = state.tasks.get(&task_id).cloned();
                            let project = task.as_ref().and_then(|t| state.projects.get(&t.project).cloned());
                            (task, project)
                        };

                        let (task, project) = match (task, project) {
                            (Some(t), Some(p)) => (t, p),
                            _ => {
                                warn!(task_id = %task_id, "cannot answer question: task or project not found");
                                continue;
                            }
                        };

                        // Spawn LLM call to avoid blocking the event loop
                        let orch_ref = orch.clone();
                        let server_ref = orch_server.clone();
                        let session_mgr_ref = orch_session_mgr.clone();
                        let question_clone = question.clone();
                        tokio::spawn(async move {
                            let context = QuestionContext {
                                task: task.clone(),
                                project,
                                question: question_clone,
                                human_present: false,
                            };

                            match orch_ref.answer_question(&context).await {
                                Ok(answer) => {
                                    info!(
                                        task_id = %task_id,
                                        answer_len = answer.len(),
                                        "orchestrator answered agent question"
                                    );

                                    // Emit HumanMessage event BEFORE sending to session.
                                    // This triggers the dispatcher's pending_answers mechanism,
                                    // which transitions the task from Question to Running on the
                                    // next dispatch tick. Without this, the task stays in Question
                                    // state indefinitely (issue #552 review comment).
                                    let human_message_event = events::Event::new(
                                        events::EventType::HumanMessage,
                                        &task_id,
                                        events::Actor::Orchestrator,
                                        serde_json::json!({
                                            "message": answer.clone(),
                                            "source": "orchestrator_answer",
                                        }),
                                    );
                                    if let Err(e) = server_ref.event_bus.publish(human_message_event).await {
                                        error!(
                                            task_id = %task_id,
                                            error = %e,
                                            "failed to emit HumanMessage event for orchestrator answer"
                                        );
                                    }

                                    // Send the answer to the agent session
                                    if let Err(e) = session_mgr_ref.send_chat(&task_id, answer.clone()).await {
                                        warn!(
                                            task_id = %task_id,
                                            error = %e,
                                            "failed to send orchestrator answer to session"
                                        );
                                    }

                                    // Emit orchestrator:response event so humans can see what was answered
                                    if let Err(e) = server_ref.emit_orchestrator_feedback(
                                        &task_id,
                                        &answer,
                                        Some("question_answer"),
                                    ).await {
                                        error!(
                                            task_id = %task_id,
                                            error = %e,
                                            "failed to emit orchestrator feedback event for question answer"
                                        );
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        task_id = %task_id,
                                        error = %e,
                                        "orchestrator failed to answer agent question"
                                    );
                                }
                            }
                        });
                        continue;
                    }
                    // Handle orchestrator chat messages from humans (bypass queue)
                    EventType::OrchestratorMessage => {
                        let message = event
                            .data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        if !message.is_empty() {
                            if let Some(ref chat) = orchestrator_chat {
                                info!(message_len = message.len(), "Received orchestrator chat message");

                                // Build context from current state
                                let context = {
                                    let mode = match orch_server.mode().await {
                                        server::Mode::Play => "Play",
                                        server::Mode::Pause => "Pause",
                                        server::Mode::Stop => "Stop",
                                    };
                                    let state = orch_server.state.read().await;
                                    ChatContext {
                                        mode: mode.to_string(),
                                        projects: state.projects.values().cloned().collect(),
                                        tasks: state.tasks.values().cloned().collect(),
                                        recent_events: Vec::new(),
                                        human_present: orch_server.is_human_present(),
                                    }
                                };

                                // Spawn LLM call to avoid blocking the event loop
                                let chat = Arc::clone(chat);
                                let history = Arc::clone(&chat_history);
                                let bus = orch_server.event_bus.clone();
                                tokio::spawn(async move {
                                    tracing::info!("Spawned orchestrator chat task, calling LLM...");
                                    let history_snapshot = history.lock().await.clone();
                                    match chat.process_message(&message, &context, &history_snapshot).await {
                                        Ok(response) => {
                                            // Update conversation history
                                            {
                                                let mut h = history.lock().await;
                                                h.push(tasks_agent::Message::user(&message));
                                                h.push(tasks_agent::Message::assistant(&response.message));
                                                // Keep history bounded
                                                if h.len() > 40 {
                                                    let start = h.len() - 40;
                                                    *h = h[start..].to_vec();
                                                }
                                            }
                                            let resp_event = Event::new(
                                                EventType::OrchestratorResponse,
                                                SYSTEM_TASK_ID,
                                                Actor::Orchestrator,
                                                serde_json::json!({
                                                    "message": response.message,
                                                }),
                                            );
                                            tracing::info!(response_len = response.message.len(), "Orchestrator chat response ready, publishing...");
                                            if let Err(e) = bus.publish(resp_event).await {
                                                tracing::error!(error = %e, "Failed to publish orchestrator response");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "Failed to process orchestrator chat message");
                                            let err_event = Event::new(
                                                EventType::OrchestratorResponse,
                                                SYSTEM_TASK_ID,
                                                Actor::Orchestrator,
                                                serde_json::json!({
                                                    "message": format!("I encountered an error processing your message: {}", e),
                                                    "error": true,
                                                }),
                                            );
                                            let _ = bus.publish(err_event).await;
                                        }
                                    }
                                });
                            } else {
                                warn!("OrchestratorChat not available (missing ANTHROPIC_API_KEY)");
                            }
                        }
                        continue; // Don't process merge queue for chat messages
                    }
                    // All other events — not relevant to the orchestrator loop
                    _ => {
                        continue;
                    }
                }
            }

            // --- Eval tick: pop one entry from the FIFO queue ---

            // On startup or mode change, seed the queue with pending entries
            // that haven't been evaluated yet or have new commits.
            if eval_queue.is_empty() {
                let state = orch_server.state.read().await;
                for entry in state.merge_queue.pending() {
                    // Check if PR needs evaluation: never evaluated or has new commits
                    if needs_eval(&evaluated_prs, &entry) && eval_queued.insert(entry.id.clone()) {
                        eval_queue.push_back(entry.id.clone());
                    }
                }
            }

            // Read current mode — idle in Stop
            let mode = orch_server.mode().await;
            if mode == server::Mode::Stop {
                continue;
            }

            // Pop one entry from the queue
            let entry_id = match eval_queue.pop_front() {
                Some(id) => {
                    eval_queued.remove(&id);
                    id
                }
                None => continue, // Nothing to evaluate
            };

            // Look up the entry — it may have been removed since queuing
            let (task_id, pr_url) = {
                let state = orch_server.state.read().await;
                match state.merge_queue.get(&entry_id) {
                    Some(e) if e.status == server::model::merge_queue::MergeStatus::Pending => {
                        (e.task_id.clone(), e.pr_url.clone())
                    }
                    _ => continue, // Entry gone or no longer pending
                }
            };

            info!(entry_id = %entry_id, queue_remaining = eval_queue.len(), "evaluating merge queue entry");

            {
            let (entry_id, task_id, pr_url) = (entry_id, task_id, pr_url);
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

                let (entry, entry_head_sha, queue_context) = {
                    let state = orch_server.state.read().await;
                    let entry = match state.merge_queue.get(&entry_id) {
                        Some(e) => e.clone(),
                        None => continue,
                    };
                    let entry_head_sha: Option<String> = entry.head_sha.clone();

                    // Build queue context: summaries of other PRs in the queue
                    // This helps the orchestrator understand dependencies between PRs
                    let mut queue_context: Vec<tasks_orchestrator::QueueEntrySummary> = state
                        .merge_queue
                        .entries()
                        .iter()
                        .filter(|other_entry| other_entry.id != entry_id) // Exclude current entry
                        .map(|other_entry| {
                            // Get task title for this entry
                            let task_title = state
                                .tasks
                                .get(&other_entry.task_id)
                                .map(|t| t.title.clone())
                                .unwrap_or_else(|| "(unknown task)".to_string());
                            tasks_orchestrator::QueueEntrySummary::from_entry(
                                other_entry,
                                &task_title,
                                other_entry.queue_position,
                            )
                        })
                        .collect();

                    // Sort by queue position (entries without position come last)
                    queue_context.sort_by(|a, b| {
                        match (a.queue_position, b.queue_position) {
                            (Some(pa), Some(pb)) => pa.cmp(&pb),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => a.queued_at.cmp(&b.queued_at),
                        }
                    });

                    (entry, entry_head_sha, queue_context)
                };

                let context = tasks_orchestrator::EvaluationContext {
                    entry,
                    task: task.clone(),
                    project,
                    queue_context,
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

                // Post evaluation comment on the GitHub PR (best-effort).
                // This makes the orchestrator's reasoning visible to the PR author.
                if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(&pr_url) {
                    let verdict = if evaluation.approved { "Approved" } else { "Rejected" };
                    let mut comment_body = format!(
                        "**Orchestrator Evaluation: {}**\n\n{}",
                        verdict, evaluation.reasoning
                    );
                    if let Some(feedback) = &evaluation.feedback {
                        comment_body.push_str(&format!(
                            "\n\n---\n**Feedback for agent:**\n{}",
                            feedback
                        ));
                    }
                    if let Err(e) = merge_github.post_issue_comment(&owner, &repo, number, &comment_body).await {
                        warn!(
                            entry_id = %entry_id,
                            pr_url = %pr_url,
                            error = %e,
                            "failed to post evaluation comment on PR (best-effort)"
                        );
                    }
                }

                // Mark this PR as evaluated so we don't re-evaluate until new commits.
                // Only record the SHA if we actually know it — if head_sha is None,
                // don't insert so reconciliation can trigger re-evaluation once the
                // SHA is discovered.
                if let Some(sha) = &entry_head_sha {
                    evaluated_prs.insert(pr_url.clone(), sha.clone());
                }

                // Act on the decision (issue #337).
                //
                // Rejections, conflict handling, and changes-requested execute
                // in both Play and Pause modes — only the actual merge-on-approval
                // is held in Pause mode. Stop mode never reaches here (filtered above).
                if evaluation.approved {
                    // Track approval (resets rejection counter)
                    if let Ok(mut tracker) = problem_tracker.lock() {
                        tracker.record_approval();
                    }

                    // Always mark the entry as approved so the UI reflects the decision
                    if let Err(e) = orch_server
                        .approve_merge_entry(&entry_id, &evaluation.reasoning)
                        .await
                    {
                        error!(entry_id = %entry_id, error = %e, "failed to approve merge entry");
                    }

                    if mode == server::Mode::Play {
                        // Execute the merge on GitHub (Play mode = continuous merge authority)
                        if let Some((owner, repo, number)) = tasks_orchestrator::parse_pr_url(&pr_url) {
                            // Transition to Merging before the API call for visibility
                            if let Err(e) = orch_server.mark_entry_merging(&entry_id, &pr_url).await {
                                error!(entry_id = %entry_id, error = %e, "failed to mark entry as merging");
                            }

                            match merge_github.merge_pull_request(&owner, &repo, number).await {
                                Ok(true) => {
                                    info!(entry_id = %entry_id, pr_url = %pr_url, "PR merged successfully");
                                    if let Err(e) = orch_server.mark_entry_merged(&entry_id, &pr_url).await {
                                        error!(entry_id = %entry_id, error = %e, "failed to mark entry as merged");
                                    }
                                }
                                Ok(false) => {
                                    // Merge failed — get detailed conflict info and triage
                                    warn!(entry_id = %entry_id, pr_url = %pr_url, "PR not mergeable, checking conflict details");

                                    // Get detailed merge status
                                    let merge_status = merge_github
                                        .check_pr_merge_status(&owner, &repo, number)
                                        .await;

                                    let conflict_info = match merge_status {
                                        Ok(status) => {
                                            let conflict_type = classify_conflict(&status);
                                            Some(ConflictInfo::new(conflict_type, format!(
                                                "Merge conflict detected: {:?}",
                                                conflict_type
                                            )).with_files(status.conflicting_files.clone()))
                                        }
                                        Err(e) => {
                                            warn!(error = %e, "failed to get PR merge status, using unknown conflict type");
                                            Some(ConflictInfo::new(
                                                ConflictType::Unknown,
                                                "Could not determine conflict type",
                                            ))
                                        }
                                    };

                                    // Mark entry as conflicted with details
                                    if let Err(e) = orch_server
                                        .mark_entry_conflict(&entry_id, &pr_url, conflict_info.clone())
                                        .await
                                    {
                                        error!(entry_id = %entry_id, error = %e, "failed to mark entry as conflict");
                                        continue;
                                    }

                                    // Triage the conflict
                                    if let Some(info) = conflict_info {
                                        let human_present = orch_server.is_human_present();
                                        let orch_mode = match mode {
                                            server::Mode::Play => OperatingMode::Play,
                                            server::Mode::Pause => OperatingMode::Pause,
                                            server::Mode::Stop => OperatingMode::Stop,
                                        };
                                        let conflict_ctx = ConflictContext {
                                            entry: context.entry.clone(),
                                            conflict_info: info,
                                            task: context.task.clone(),
                                            project: context.project.clone(),
                                            human_present,
                                            mode: orch_mode,
                                        };

                                        match orchestrator.triage_conflict(&conflict_ctx).await {
                                            Ok(triage) => {
                                                info!(
                                                    entry_id = %entry_id,
                                                    resolution = ?triage.resolution,
                                                    reasoning = %triage.reasoning,
                                                    "conflict triage complete"
                                                );

                                                // Act on the triage decision
                                                match triage.resolution {
                                                    ConflictResolution::Rebase => {
                                                        // Mechanical resolution via GitHub update-branch API (spec §7.4)
                                                        info!(entry_id = %entry_id, "attempting mechanical rebase via update-branch API");
                                                        match merge_github.update_branch(&owner, &repo, number).await {
                                                            Ok(true) => {
                                                                info!(entry_id = %entry_id, pr_url = %pr_url, "branch updated successfully, clearing conflict");
                                                                // Clear conflict status so entry returns to pending
                                                                if let Err(e) = orch_server.clear_entry_conflict(&entry_id).await {
                                                                    error!(entry_id = %entry_id, error = %e, "failed to clear conflict after successful rebase");
                                                                }
                                                                // Emit event for audit trail
                                                                let event = Event::new(
                                                                    EventType::OrchestratorFeedback,
                                                                    &task_id,
                                                                    Actor::Orchestrator,
                                                                    serde_json::json!({
                                                                        "action": "mechanical_rebase",
                                                                        "entry_id": entry_id,
                                                                        "pr_url": pr_url,
                                                                        "success": true,
                                                                    }),
                                                                );
                                                                if let Err(e) = orch_server.event_bus.publish(event).await {
                                                                    error!(error = %e, "failed to emit rebase success event");
                                                                }
                                                                // Remove from evaluated set so it gets re-evaluated next cycle
                                                                evaluated_prs.remove(&pr_url);
                                                            }
                                                            Ok(false) => {
                                                                // Update failed (conflicts can't be auto-resolved) — fall back to agent
                                                                warn!(entry_id = %entry_id, pr_url = %pr_url, "update-branch failed, falling back to agent re-engagement");
                                                                let feedback = format!(
                                                                    "Automatic rebase failed. Please manually rebase your branch on the latest main branch and resolve any conflicts."
                                                                );
                                                                if let Err(e) = orch_server.reengage_for_conflict(&entry_id, &feedback).await {
                                                                    error!(error = %e, "failed to re-engage agent after rebase failure");
                                                                }
                                                            }
                                                            Err(e) => {
                                                                error!(entry_id = %entry_id, error = %e, "update-branch API error, falling back to agent");
                                                                let feedback = format!(
                                                                    "Automatic rebase failed due to an error. Please manually rebase your branch on the latest main branch."
                                                                );
                                                                if let Err(e) = orch_server.reengage_for_conflict(&entry_id, &feedback).await {
                                                                    error!(error = %e, "failed to re-engage agent after rebase error");
                                                                }
                                                            }
                                                        }
                                                    }
                                                    ConflictResolution::AutoResolve => {
                                                        // Trivial conflicts (lock files, generated code) — try update-branch first
                                                        // If that fails, the agent needs to resolve them manually
                                                        info!(entry_id = %entry_id, "attempting auto-resolve via update-branch API");
                                                        match merge_github.update_branch(&owner, &repo, number).await {
                                                            Ok(true) => {
                                                                info!(entry_id = %entry_id, pr_url = %pr_url, "branch updated, trivial conflict resolved");
                                                                if let Err(e) = orch_server.clear_entry_conflict(&entry_id).await {
                                                                    error!(entry_id = %entry_id, error = %e, "failed to clear conflict after auto-resolve");
                                                                }
                                                                let event = Event::new(
                                                                    EventType::OrchestratorFeedback,
                                                                    &task_id,
                                                                    Actor::Orchestrator,
                                                                    serde_json::json!({
                                                                        "action": "auto_resolve",
                                                                        "entry_id": entry_id,
                                                                        "pr_url": pr_url,
                                                                        "success": true,
                                                                    }),
                                                                );
                                                                if let Err(e) = orch_server.event_bus.publish(event).await {
                                                                    error!(error = %e, "failed to emit auto-resolve success event");
                                                                }
                                                                evaluated_prs.remove(&pr_url);
                                                            }
                                                            Ok(false) | Err(_) => {
                                                                // Can't auto-resolve — re-engage agent with specific instructions
                                                                warn!(entry_id = %entry_id, "auto-resolve failed, re-engaging agent");
                                                                let feedback = triage.agent_feedback.unwrap_or_else(|| {
                                                                    "Your branch has conflicts in lock files or generated code. Please regenerate these files after rebasing on the latest main branch.".to_string()
                                                                });
                                                                if let Err(e) = orch_server.reengage_for_conflict(&entry_id, &feedback).await {
                                                                    error!(error = %e, "failed to re-engage for auto-resolve fallback");
                                                                }
                                                            }
                                                        }
                                                    }
                                                    ConflictResolution::ReengageAgent => {
                                                        if let Some(feedback) = triage.agent_feedback {
                                                            if let Err(e) = orch_server
                                                                .reengage_for_conflict(&entry_id, &feedback)
                                                                .await
                                                            {
                                                                error!(error = %e, "failed to re-engage for conflict");
                                                            }
                                                        }
                                                    }
                                                    ConflictResolution::SurfaceToHuman => {
                                                        // Emit escalation event for human review
                                                        let event = Event::new(
                                                            EventType::OrchestratorEscalation,
                                                            &task_id,
                                                            Actor::Orchestrator,
                                                            serde_json::json!({
                                                                "action": "conflict_needs_human",
                                                                "entry_id": entry_id,
                                                                "pr_url": pr_url,
                                                                "reasoning": triage.reasoning,
                                                            }),
                                                        );
                                                        if let Err(e) = orch_server.event_bus.publish(event).await {
                                                            error!(error = %e, "failed to emit escalation");
                                                        }
                                                    }
                                                    ConflictResolution::RetryLater => {
                                                        // Will be retried on next iteration
                                                        debug!(entry_id = %entry_id, "conflict triage: retry later");
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!(entry_id = %entry_id, error = %e, "conflict triage failed");
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!(entry_id = %entry_id, pr_url = %pr_url, error = %e, "failed to merge PR on GitHub, reverting to Approved for retry");
                                    if let Err(e) = orch_server.revert_entry_to_approved(&entry_id, &pr_url).await {
                                        error!(entry_id = %entry_id, error = %e, "failed to revert entry to Approved");
                                    }
                                }
                            }
                        } else {
                            warn!(entry_id = %entry_id, pr_url = %pr_url, "could not parse PR URL for merge execution");
                        }
                    } else {
                        // Pause mode: entry is marked approved (above) but NOT merged.
                        // The human can flush approved entries via the flush endpoint.
                        info!(
                            entry_id = %entry_id,
                            pr_url = %pr_url,
                            "Pause mode: entry approved but merge held for human flush"
                        );
                    }
                } else {
                    // Rejected — always act on rejections regardless of mode (issue #337).
                    // Rejections, conflict re-engagement, and changes-requested must not
                    // be blocked by Pause mode, only actual merges are held.

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

                        // Record rejection in cooldown tracker to prevent re-queuing (issue #439)
                        if let Some(sha) = &entry_head_sha {
                            let mut cooldown = orch_rejected_cooldown.lock().unwrap();
                            cooldown.record(pr_url.clone(), sha.clone());
                            debug!(
                                pr_url = %pr_url,
                                head_sha = %sha,
                                "recorded PR rejection in cooldown tracker (issue closed)"
                            );
                        } else {
                            warn!("Cannot record rejection cooldown for {}: entry has no head_sha", pr_url);
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

                        // Delete the remote branch so the agent starts fresh (issue #143).
                        // Without this, the agent would find the old branch and resubmit
                        // the same work instead of starting over.
                        let branch = format!("tasks/{}", task_id);
                        let repo_parts: Vec<&str> = context.project.repo.split('/').collect();
                        if repo_parts.len() == 2 {
                            match merge_github.delete_branch(repo_parts[0], repo_parts[1], &branch).await {
                                Ok(true) => {
                                    info!(
                                        task_id = %task_id,
                                        branch = %branch,
                                        "deleted remote branch for re-dispatch"
                                    );
                                }
                                Ok(false) => {
                                    // Branch didn't exist — that's fine
                                    debug!(
                                        task_id = %task_id,
                                        branch = %branch,
                                        "branch did not exist on remote"
                                    );
                                }
                                Err(e) => {
                                    // Log but don't fail — the task can still be re-dispatched.
                                    // The agent may find old work, but that's better than
                                    // blocking the entire rejection flow.
                                    warn!(
                                        task_id = %task_id,
                                        branch = %branch,
                                        error = %e,
                                        "failed to delete remote branch for re-dispatch"
                                    );
                                }
                            }
                        } else {
                            warn!(
                                task_id = %task_id,
                                repo = %context.project.repo,
                                "invalid repo format, cannot delete branch"
                            );
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
                        // Feedback is now stored on the task (issue #423) and will be
                        // delivered to the agent via the prompt when re-dispatched.

                        // Record rejection in cooldown tracker to prevent re-queuing (issue #439)
                        if let Some(sha) = &entry_head_sha {
                            let mut cooldown = orch_rejected_cooldown.lock().unwrap();
                            cooldown.record(pr_url.clone(), sha.clone());
                            debug!(
                                pr_url = %pr_url,
                                head_sha = %sha,
                                "recorded PR rejection in cooldown tracker"
                            );
                        } else {
                            warn!("Cannot record rejection cooldown for {}: entry has no head_sha", pr_url);
                        }
                    }
                }
            } // end single-entry evaluation block

            // Cleanup terminal merge queue entries (issue #132, #282, #438).
            // This removes Merged, Rejected, and stale Conflict entries to prevent unbounded growth.
            // Merged entries have a 5-minute cooldown to prevent race conditions with GitHub API
            // propagation delays (issue #438).
            let chrono_max_age = match chrono::Duration::from_std(conflict_max_age) {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        error = %e,
                        fallback = "24h",
                        "conflict_max_age exceeds chrono::Duration range, falling back to 24h"
                    );
                    chrono::Duration::hours(24)
                }
            };
            let conflict_cutoff = chrono::Utc::now() - chrono_max_age;
            // 5-minute cooldown for merged entries to handle GitHub API propagation delay
            let merged_cutoff = chrono::Utc::now() - chrono::Duration::minutes(5);
            orch_server.cleanup_merge_queue(Some(merged_cutoff), Some(conflict_cutoff)).await;

            // Cleanup stale entries from rejected PR cooldown tracker (issue #439)
            {
                let mut cooldown = orch_rejected_cooldown.lock().unwrap();
                cooldown.cleanup(REJECTED_PR_COOLDOWN);
            }
        }
    });

    // --- 8c. Spawn work queue health check loop ---
    //
    // Periodically checks for stale work claims (dead or timed-out containers)
    // and reclaims them so the work can be re-dispatched.

    let health_work_queue = work_queue.clone();
    let health_session_mgr = session_manager.clone();
    let mut health_shutdown_rx = shutdown_tx.subscribe();

    let health_check_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(health_check_interval);

        loop {
            tokio::select! {
                _ = health_shutdown_rx.recv() => {
                    info!("health check loop received shutdown signal");
                    break;
                }
                _ = interval.tick() => {}
            }

            let mut queue = health_work_queue.write().await;

            // Use sync version that won't block if lock is held
            let session_mgr = health_session_mgr.clone();
            let is_alive = |container_id: &str| -> bool {
                session_mgr.has_container_sync(container_id)
            };

            match queue.health_check(is_alive) {
                Ok(reclaimed) => {
                    for item in reclaimed {
                        info!(
                            work_id = %item.work_id,
                            previous_container = %item.previous_container_id,
                            reason = %item.reason,
                            "reclaimed stale work"
                        );
                    }
                }
                Err(e) => {
                    error!(error = %e, "health check failed");
                }
            }
        }
    });

    // --- 8d. Spawn workspace cleanup loop (spec §10.3) ---
    //
    // Periodically scans for workspaces eligible for cleanup:
    // - Tasks in terminal states (Completed, Failed, Cancelled)
    // - Stale/idle workspaces (no activity beyond threshold)
    //
    // For each candidate, the loop destroys the container (if still running)
    // and clears the workspace_id from the task. Event logs are retained.

    let cleanup_server = server.clone();
    let cleanup_session_mgr = session_manager.clone();
    let cleanup_interval = config.cleanup_interval;
    let stale_threshold = config.workspace_stale_threshold;
    let mut cleanup_shutdown_rx = shutdown_tx.subscribe();

    let cleanup_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(cleanup_interval);

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cleanup_shutdown_rx.recv() => {
                    info!("cleanup loop received shutdown signal");
                    break;
                }
            }

            // Get cleanup candidates
            let candidates = cleanup_server
                .get_workspace_cleanup_candidates(stale_threshold)
                .await;

            if candidates.is_empty() {
                continue;
            }

            debug!(
                count = candidates.len(),
                "workspace cleanup: found candidates"
            );

            for candidate in candidates {
                // Check if there's an active session for this task — if so, skip.
                // The session manager handles its own cleanup when sessions end.
                if cleanup_session_mgr.has_session(&candidate.task_id).await {
                    debug!(
                        task_id = %candidate.task_id,
                        "workspace cleanup: skipping, session still active"
                    );
                    continue;
                }

                // Clear the workspace_id from the task and emit event
                if let Err(e) = cleanup_server
                    .clear_workspace_id(&candidate.task_id, &candidate.reason)
                    .await
                {
                    warn!(
                        task_id = %candidate.task_id,
                        error = %e,
                        "workspace cleanup: failed to clear workspace_id"
                    );
                }
            }
        }
    });

    // --- 8e. Event log compaction loop (#470) ---
    //
    // Periodically compact event logs to enforce retention limits and clean up
    // orphaned task directories, preventing unbounded storage growth.

    let compaction_event_bus = server.event_bus.clone();
    let mut compaction_shutdown_rx = shutdown_tx.subscribe();

    let compaction_handle = tokio::spawn(async move {
        // Run compaction every hour.
        let mut interval = tokio::time::interval(Duration::from_secs(3600));

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = compaction_shutdown_rx.recv() => {
                    info!("compaction loop received shutdown signal");
                    break;
                }
            }

            match compaction_event_bus.compact().await {
                Ok(removed) if removed > 0 => {
                    info!(removed, "event log compaction removed events");
                }
                Err(e) => {
                    warn!(error = %e, "event log compaction failed");
                }
                _ => {}
            }

            match compaction_event_bus.cleanup_orphaned_tasks().await {
                Ok(removed) if removed > 0 => {
                    info!(removed, "cleaned up orphaned task directories");
                }
                Err(e) => {
                    warn!(error = %e, "orphaned task cleanup failed");
                }
                _ => {}
            }
        }
    });

    // --- 8f. Spawn stop mode listener (spec §6.1) ---
    //
    // When mode changes to Stop, terminate all running sessions.
    // This is event-driven so that any mode change source (web, CLI, orchestrator)
    // triggers session termination consistently.

    let stop_session_mgr = session_manager.clone();
    let stop_event_bus = server.event_bus.clone();
    let mut stop_mode_shutdown_rx = shutdown_tx.subscribe();

    let stop_mode_handle = tokio::spawn(async move {
        let mut rx = stop_event_bus.subscribe();
        loop {
            let event = tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(e) => e,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            error!(
                                skipped = n,
                                "stop mode listener lagged — {n} events dropped, may miss SystemModeStop event"
                            );
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                _ = stop_mode_shutdown_rx.recv() => {
                    info!("stop mode listener received shutdown signal");
                    break;
                }
            };

            // Only handle SystemModeStop events
            if event.event_type == EventType::SystemModeStop {
                // Give sessions 5 seconds to stop gracefully before force-destroying containers
                let timeout = std::time::Duration::from_secs(5);
                let stopped = stop_session_mgr.stop_all_with_timeout(timeout).await;
                if stopped > 0 {
                    info!(
                        stopped_sessions = stopped,
                        "terminated sessions for Stop mode (event-driven)"
                    );
                }
            }
        }
    });

    // --- 8e. Spawn orchestrator think loop ---
    //
    // Periodic reasoning pass: the orchestrator surveys system state and
    // recent events, identifies patterns, and emits narration (thoughts)
    // plus state change requests. This runs on a fixed interval (~30s),
    // independent of the evaluation loop.
    //
    // The think loop collects events between ticks and passes them to
    // think() as recent_events so the orchestrator can see what happened.

    let think_server = server.clone();
    let think_event_bus = server.event_bus.clone();
    let mut think_shutdown_rx = shutdown_tx.subscribe();

    let think_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        let mut event_rx = think_event_bus.subscribe();
        let mut recent_events: Vec<Event> = Vec::new();
        let mut last_think_at: Option<chrono::DateTime<chrono::Utc>> = None;

        loop {
            // Wait for next tick, collecting events in between
            tokio::select! {
                _ = interval.tick() => {}
                _ = think_shutdown_rx.recv() => {
                    info!("think loop received shutdown signal");
                    break;
                }
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => {
                            // Buffer events for the next think() pass.
                            // Skip high-frequency noise (scheduler ticks, accounting, etc.)
                            let dominated = event.event_type.matches("system:scheduler:*")
                                || event.event_type.matches("system:accounting:*")
                                || event.event_type.matches("automation:run:output");
                            if !dominated {
                                recent_events.push((*event).clone());
                            }
                            // Cap buffer to prevent unbounded growth
                            if recent_events.len() > 200 {
                                recent_events.drain(..100);
                            }
                            continue; // Keep collecting until the interval fires
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "think loop lagged — {n} events dropped");
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            }

            // Skip think() in Stop mode
            let mode = think_server.mode().await;
            if mode == server::Mode::Stop {
                recent_events.clear();
                continue;
            }

            // Build SystemContext snapshot
            let context = {
                let orch_mode = match think_server.mode().await {
                    server::Mode::Play => OperatingMode::Play,
                    server::Mode::Pause => OperatingMode::Pause,
                    server::Mode::Stop => OperatingMode::Stop,
                };
                let state = think_server.state.read().await;
                SystemContext {
                    mode: orch_mode,
                    projects: state.projects.values().cloned().collect(),
                    tasks: state.tasks.values().cloned().collect(),
                    merge_queue: state.merge_queue.entries().to_vec(),
                    human_present: think_server.is_human_present(),
                    recent_events: recent_events.clone(),
                    last_think_at,
                }
            };

            // Run the orchestrator's think pass
            match think_orch.think(&context).await {
                Ok(actions) => {
                    for action in actions {
                        match action {
                            OrchestratorAction::EmitThought(message) => {
                                let event = Event::new(
                                    EventType::OrchestratorThought,
                                    SYSTEM_TASK_ID,
                                    Actor::Orchestrator,
                                    serde_json::json!({ "message": message }),
                                );
                                if let Err(e) = think_server.event_bus.publish(event).await {
                                    error!(error = %e, "failed to publish orchestrator thought");
                                }
                            }
                            OrchestratorAction::UpdateTaskState { task_id, state } => {
                                info!(task_id = %task_id, state = ?state, "orchestrator requested task state change");
                                if let Err(e) = think_server
                                    .set_task_state(&task_id, state, Actor::Orchestrator)
                                    .await
                                {
                                    error!(task_id = %task_id, error = %e, "failed to update task state from think()");
                                }
                            }
                            OrchestratorAction::PrioritizeTask { task_id, reason } => {
                                info!(task_id = %task_id, reason = %reason, "orchestrator requested task prioritization (not yet implemented)");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "orchestrator think() failed");
                }
            }

            last_think_at = Some(chrono::Utc::now());
            recent_events.clear();
        }
    });

    // --- 9. Optionally spawn web server ---

    // Create update trigger channel (shared between API and auto-apply)
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel::<()>(1);

    let web_handle = if config.web {
        // Initialize completions service for fast Haiku-based completions.
        let completions_service = tasks_agent::CompletionsService::from_env()
            .map(Arc::new)
            .ok();
        if completions_service.is_some() {
            info!("completions service available");
        } else {
            warn!("completions service unavailable (ANTHROPIC_API_KEY not set)");
        }

        // Initialize automation executor for running automation prompts.
        let automation_executor = server::AutomationExecutor::from_env()
            .map(Arc::new)
            .ok();
        if automation_executor.is_some() {
            info!("automation executor available");
        } else {
            warn!("automation executor unavailable (ANTHROPIC_API_KEY not set)");
        }

        let api_state = crate::web::ApiState {
            server: server.clone(),
            max_sessions: config.max_sessions,
            session_manager: Some(session_manager.clone()),
            completions_service,
            automation_executor,
            update_state: update_state.clone(),
            update_tx: update_tx.clone(),
            blocked_repos: config.blocked_repos.clone(),
            blocked_orgs: config.blocked_orgs.clone(),
            automation_soft_limit: config.automation_soft_limit,
            automation_hard_limit: config.automation_hard_limit,
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
        let mut web_shutdown_rx = shutdown_tx.subscribe();
        Some(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = web_shutdown_rx.recv().await;
                    info!("web server received shutdown signal");
                })
                .await
                .ok();
        }))
    } else {
        None
    };

    // --- 10. Wait for shutdown ---
    //
    // The server can shut down due to:
    // - Ctrl-C (normal shutdown)
    // - Update trigger (exit with code 100 for restart)

    // If auto-apply is enabled, spawn a task that triggers update when available
    let auto_apply_handle = if config.update_auto_apply && config.update_check_enabled {
        let state = update_state.clone();
        let tx = update_tx.clone();
        let check_interval = config.update_check_interval;
        let mut auto_apply_shutdown_rx = shutdown_tx.subscribe();

        Some(tokio::spawn(async move {
            // Wait a bit before first check to let system stabilize
            tokio::select! {
                _ = tokio::time::sleep(check_interval) => {}
                _ = auto_apply_shutdown_rx.recv() => {
                    info!("auto-apply received shutdown signal");
                    return;
                }
            }

            loop {
                if state.is_available().await && !state.is_applying() {
                    info!("auto-apply: update available, triggering restart");
                    let _ = tx.send(()).await;
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    _ = auto_apply_shutdown_rx.recv() => {
                        info!("auto-apply received shutdown signal");
                        return;
                    }
                }
            }
        }))
    } else {
        None
    };

    // Store config values we need after the loop
    let data_dir = config.data_dir.clone();
    let session_timeout = config.update_session_timeout;

    // Wait for either Ctrl-C or update trigger
    let shutdown_reason = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C, shutting down");
            RunResult::Normal
        }
        _ = update_rx.recv() => {
            info!("update triggered, preparing for restart");
            RunResult::UpdateRestart
        }
    };

    // If this is an update restart, we need to:
    // 1. Stop active sessions (with configured timeout)
    // 2. Write the update scope file
    if shutdown_reason == RunResult::UpdateRestart {
        update_state.set_applying(true);

        // Get update info for scope file
        let scope = if let Some(info) = update_state.get_info().await {
            info.scope.clone()
        } else {
            update::RebuildScope::All
        };

        // Stop all sessions directly with the configured timeout.
        // We bypass set_mode(Stop) because the stop_mode_handle has a hardcoded
        // 5-second timeout that would race with and override our configured timeout.
        info!(timeout = ?session_timeout, "stopping sessions for update");
        let stopped = session_manager.stop_all_with_timeout(session_timeout).await;
        if stopped > 0 {
            info!(stopped_sessions = stopped, "sessions stopped for update");
        }

        // Write update scope file
        if let Err(e) = update::write_update_scope(&data_dir, &scope) {
            error!(error = %e, "failed to write update scope file");
        }
    }

    // Stop all sessions and destroy their containers
    session_manager.destroy_all().await;

    // Signal all tasks to shut down gracefully
    info!("sending shutdown signal to all tasks");
    shutdown_tx.send(()).ok();

    // Collect all handles for the grace period wait
    let mut handles: Vec<tokio::task::JoinHandle<()>> = vec![
        poll_handle,
        automation_scheduler_handle,
        automation_listener_handle,
        dispatch_handle,
        event_handler_handle,
        orchestrator_handle,
        think_handle,
        watchdog_handle,
        health_check_handle,
        cleanup_handle,
        compaction_handle,
        stop_mode_handle,
    ];
    if let Some(h) = update_handle {
        handles.push(h);
    }
    if let Some(h) = auto_apply_handle {
        handles.push(h);
    }
    if let Some(h) = web_handle {
        handles.push(h);
    }

    // Give tasks a grace period to finish in-flight work, then abort stragglers
    let grace_period = Duration::from_secs(5);
    match tokio::time::timeout(grace_period, futures::future::join_all(&mut handles)).await {
        Ok(_) => {
            info!("all tasks shut down gracefully");
        }
        Err(_) => {
            warn!("grace period expired, aborting remaining tasks");
            for handle in &handles {
                handle.abort();
            }
        }
    }

    info!("shutdown complete");
    Ok(shutdown_reason)
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
/// Uses the cached workflow config from the watcher (spec §14.3) when available,
/// which is populated by the poll loop. Falls back to fetching from GitHub
/// if not cached.
///
/// 1. Checks the workflow config cache
/// 2. Extracts `dispatch.progress_threshold` (spec §13.1, §14.2)
/// 3. Fetches the system prompt file if configured
/// 4. Returns the settings, with defaults for any unavailable fields
///
/// Errors are logged but don't fail dispatch — the session continues with
/// defaults for any fields that couldn't be loaded.
async fn load_workflow_settings_for_project(
    project: Option<&models::project::Project>,
    github_token: &str,
    config_watcher: &WorkflowConfigWatcher,
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

    // Get cached workflow config (spec §14.3)
    // The poll loop refreshes this periodically, so it should usually be cached.
    let workflow_config = config_watcher.get_config(&project.id).await;

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
    // Note: System prompts are still fetched fresh since they may change independently
    // of workflow.toml. A future optimization could cache these as well.
    let client = GitHubClient::new(github_token);
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
