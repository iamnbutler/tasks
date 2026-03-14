//! App configuration — reads from environment variables.

use std::time::Duration;

/// Top-level app configuration.
pub struct AppConfig {
    /// GitHub personal access token.
    pub github_token: String,
    /// Global max concurrent sessions (default: 5).
    pub max_sessions: u32,
    /// GitHub poll interval (default: 60s).
    pub poll_interval: Duration,
    /// Dispatch tick interval (default: 30s).
    pub dispatch_interval: Duration,
    /// Container image for sessions.
    pub container_image: String,
    /// Session soft time limit (default: 1h).
    pub session_soft_limit: Duration,
    /// Session hard time limit (default: 1h15m).
    pub session_hard_limit: Duration,
}

impl AppConfig {
    /// Read configuration from environment variables.
    pub fn from_env() -> Result<Self, String> {
        let github_token = std::env::var("GITHUB_TOKEN")
            .map_err(|_| "GITHUB_TOKEN environment variable not set".to_string())?;

        let container_image = std::env::var("TASKS_CONTAINER_IMAGE")
            .unwrap_or_else(|_| "tasks-agent:latest".to_string());

        let max_sessions = std::env::var("TASKS_MAX_SESSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let poll_interval_secs = std::env::var("TASKS_POLL_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60u64);

        let dispatch_interval_secs = std::env::var("TASKS_DISPATCH_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30u64);

        Ok(Self {
            github_token,
            max_sessions,
            poll_interval: Duration::from_secs(poll_interval_secs),
            dispatch_interval: Duration::from_secs(dispatch_interval_secs),
            container_image,
            session_soft_limit: Duration::from_secs(3600),
            session_hard_limit: Duration::from_secs(4500),
        })
    }
}
