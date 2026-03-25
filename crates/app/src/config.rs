//! App configuration — reads from environment variables.

use std::time::Duration;

/// Parse a comma-separated env var into a lowercased, trimmed list of non-empty strings.
fn parse_csv_env(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Load blocklist from env and check a repo against it.
/// Returns an error message if blocked, `None` if allowed.
pub fn check_blocklist(repo: &str) -> Option<String> {
    let blocked_repos = parse_csv_env("BLOCKED_REPOS");
    let blocked_orgs = parse_csv_env("BLOCKED_ORGS");
    AppConfig::check_repo_blocked(&blocked_repos, &blocked_orgs, repo)
}

/// Top-level app configuration.
pub struct AppConfig {
    /// Data directory (default: `~/.local/state/tasks`).
    pub data_dir: String,
    /// GitHub personal access token.
    pub github_token: String,
    /// Global max concurrent sessions (default: 5).
    pub max_sessions: u32,
    /// Whether the update checker is enabled (default: true).
    pub update_check_enabled: bool,
    /// Interval between update checks (default: 300s).
    pub update_check_interval: Duration,
    /// Whether to automatically apply updates when detected (default: false).
    pub update_auto_apply: bool,
    /// Timeout for waiting for sessions to drain before update (default: 300s).
    pub update_session_timeout: Duration,
    /// Default max sessions per project when no workflow.toml override exists (default: 1).
    pub max_sessions_per_project: u32,
    /// Maximum retry attempts for failed tasks (default: 3, spec §13.2/§14.1).
    pub max_retries: u32,
    /// GitHub poll interval (default: 60s).
    pub poll_interval: Duration,
    /// Dispatch tick interval (default: 30s).
    pub dispatch_interval: Duration,
    /// Orchestrator evaluation interval — how often the orchestrator pops one
    /// entry from its FIFO queue for evaluation (default: 15s, spec §7.1).
    pub orchestrator_eval_interval: Duration,
    /// Container image for sessions.
    pub container_image: String,
    /// Container memory limit (default: 8G).
    pub container_memory: String,
    /// Session soft time limit (default: 1h).
    pub session_soft_limit: Duration,
    /// Session hard time limit (default: 1h15m).
    pub session_hard_limit: Duration,
    /// Minimum session duration to count as "progress" (default: 60s, spec §13.1).
    pub progress_threshold: Duration,
    /// Memory usage percentage at which to warn (default: 75%).
    pub memory_warn_pct: u8,
    /// Memory usage percentage at which to pause dispatch (default: 85%).
    pub memory_soft_limit_pct: u8,
    /// Memory usage percentage at which to emergency-stop sessions (default: 92%).
    pub memory_hard_limit_pct: u8,
    /// Workspace stale threshold — idle workspaces older than this are cleaned up
    /// (default: 7 days, spec §10.3).
    pub workspace_stale_threshold: Duration,
    /// Workspace cleanup scan interval (default: 15 minutes).
    pub cleanup_interval: Duration,
    /// Max age for conflict entries before they're eligible for cleanup
    /// (default: 24 hours). See issue #282.
    pub conflict_max_age: Duration,
    /// Whether to run the web UI.
    pub web: bool,
    /// Web server port (default: 4800).
    pub web_port: u16,
    /// Blocked repositories (`owner/repo` patterns, from `BLOCKED_REPOS`).
    pub blocked_repos: Vec<String>,
    /// Blocked organizations (from `BLOCKED_ORGS`).
    pub blocked_orgs: Vec<String>,
}

impl AppConfig {
    /// Read configuration from `.env` file (if present) and environment variables.
    pub fn from_env() -> Result<Self, String> {
        // Load .env file if it exists — doesn't error if missing.
        dotenvy::dotenv().ok();
        let data_dir = std::env::var("TASKS_DATA_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{home}/.local/state/tasks")
        });

        let github_token = std::env::var("GITHUB_TOKEN")
            .map_err(|_| "GITHUB_TOKEN environment variable not set".to_string())?;

        let container_image = std::env::var("TASKS_CONTAINER_IMAGE")
            .unwrap_or_else(|_| "tasks-agent:latest".to_string());

        let container_memory =
            std::env::var("TASKS_CONTAINER_MEMORY").unwrap_or_else(|_| "8G".to_string());

        let max_sessions = std::env::var("TASKS_MAX_SESSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let max_sessions_per_project = std::env::var("TASKS_MAX_SESSIONS_PER_PROJECT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        let max_retries = std::env::var("TASKS_MAX_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(server::DEFAULT_MAX_RETRIES);

        let poll_interval_secs = std::env::var("TASKS_POLL_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60u64);

        let dispatch_interval_secs = std::env::var("TASKS_DISPATCH_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30u64);

        let orchestrator_eval_interval_secs = std::env::var("TASKS_ORCHESTRATOR_EVAL_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15u64);

        let progress_threshold_secs = std::env::var("TASKS_PROGRESS_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60u64);

        let memory_warn_pct = std::env::var("TASKS_MEMORY_WARN_PCT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(75u8);

        let memory_soft_limit_pct = std::env::var("TASKS_MEMORY_SOFT_LIMIT_PCT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(85u8);

        let memory_hard_limit_pct = std::env::var("TASKS_MEMORY_HARD_LIMIT_PCT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(92u8);

        if !(memory_warn_pct < memory_soft_limit_pct && memory_soft_limit_pct < memory_hard_limit_pct) {
            return Err(format!(
                "Memory thresholds must be ordered: warn ({memory_warn_pct}) < soft ({memory_soft_limit_pct}) < hard ({memory_hard_limit_pct})"
            ));
        }

        let workspace_stale_threshold_secs = std::env::var("TASKS_WORKSPACE_STALE_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(server::DEFAULT_STALE_THRESHOLD_SECS);

        let cleanup_interval_secs = std::env::var("TASKS_CLEANUP_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(server::DEFAULT_CLEANUP_INTERVAL_SECS);

        let conflict_max_age_secs = std::env::var("TASKS_CONFLICT_MAX_AGE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(server::DEFAULT_CONFLICT_MAX_AGE_SECS);

        // Update checker configuration
        let update_check_enabled = std::env::var("TASKS_UPDATE_CHECK_ENABLED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(true);

        let update_check_interval_secs = std::env::var("TASKS_UPDATE_CHECK_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300u64);

        let update_auto_apply = std::env::var("TASKS_UPDATE_AUTO_APPLY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(false);

        let update_session_timeout_secs = std::env::var("TASKS_UPDATE_SESSION_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300u64);

        let blocked_repos = parse_csv_env("BLOCKED_REPOS");
        let blocked_orgs = parse_csv_env("BLOCKED_ORGS");

        Ok(Self {
            data_dir,
            github_token,
            max_sessions,
            max_sessions_per_project,
            max_retries,
            poll_interval: Duration::from_secs(poll_interval_secs),
            dispatch_interval: Duration::from_secs(dispatch_interval_secs),
            orchestrator_eval_interval: Duration::from_secs(orchestrator_eval_interval_secs),
            container_image,
            container_memory,
            session_soft_limit: Duration::from_secs(3600),
            session_hard_limit: Duration::from_secs(4500),
            progress_threshold: Duration::from_secs(progress_threshold_secs),
            memory_warn_pct,
            memory_soft_limit_pct,
            memory_hard_limit_pct,
            workspace_stale_threshold: Duration::from_secs(workspace_stale_threshold_secs),
            cleanup_interval: Duration::from_secs(cleanup_interval_secs),
            conflict_max_age: Duration::from_secs(conflict_max_age_secs),
            web: false, // set by CLI flag
            web_port: std::env::var("TASKS_WEB_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4800),
            update_check_enabled,
            update_check_interval: Duration::from_secs(update_check_interval_secs),
            update_auto_apply,
            update_session_timeout: Duration::from_secs(update_session_timeout_secs),
            blocked_repos,
            blocked_orgs,
        })
    }

    /// Check if a repo (`owner/repo`) is blocked. Returns an error message if blocked.
    pub fn check_repo_blocked(blocked_repos: &[String], blocked_orgs: &[String], repo: &str) -> Option<String> {
        let repo_lower = repo.to_lowercase();
        if blocked_repos.contains(&repo_lower) {
            return Some(format!("repository '{repo}' is blocked"));
        }
        if let Some(org) = repo_lower.split('/').next() {
            if blocked_orgs.iter().any(|o| o == &org) {
                return Some(format!("organization '{org}' is blocked"));
            }
        }
        None
    }
}
