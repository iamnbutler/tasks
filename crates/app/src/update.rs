//! Self-update mechanism for the Tasks platform.
//!
//! This module implements a background update checker that:
//! 1. Periodically fetches from origin/main to detect new commits
//! 2. Analyzes changed files to determine rebuild scope
//! 3. Coordinates graceful shutdown when updates are ready
//!
//! The server exits with code 100 when an update is ready, signaling
//! the wrapper script to pull, rebuild, and restart.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};

/// Rebuild scope indicating which components need rebuilding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RebuildScope {
    /// Server binary needs rebuilding (any crates/** changes except supervisor).
    pub server: bool,
    /// Container image needs rebuilding (supervisor, Dockerfile, Makefile).
    pub container: bool,
    /// Frontend needs rebuilding (web/** changes).
    pub frontend: bool,
}

impl RebuildScope {
    /// Returns true if any component needs rebuilding.
    pub fn needs_rebuild(&self) -> bool {
        self.server || self.container || self.frontend
    }

    /// Serialize to a string for writing to .update-scope file.
    pub fn to_scope_string(&self) -> String {
        let mut parts = Vec::new();
        if self.server {
            parts.push("server");
        }
        if self.container {
            parts.push("container");
        }
        if self.frontend {
            parts.push("frontend");
        }
        parts.join(",")
    }

    /// Parse from a scope string (comma-separated).
    /// Used by wrapper script to read `.update-scope` file.
    #[allow(dead_code)]
    pub fn from_scope_string(s: &str) -> Self {
        let parts: HashSet<&str> = s.split(',').map(|p| p.trim()).collect();
        Self {
            server: parts.contains("server"),
            container: parts.contains("container"),
            frontend: parts.contains("frontend"),
        }
    }
}

/// State of the update checker.
#[derive(Debug, Clone, Default)]
pub struct UpdateState {
    /// Whether an update is available.
    pub update_available: bool,
    /// The current HEAD commit (short hash).
    pub current_commit: Option<String>,
    /// The target commit on origin/main (short hash).
    pub target_commit: Option<String>,
    /// Number of commits behind origin/main.
    pub commits_behind: u32,
    /// Rebuild scope if update is available.
    pub scope: RebuildScope,
    /// Last error message if any.
    pub last_error: Option<String>,
    /// Whether an update is currently being applied.
    pub applying: bool,
}

/// The update checker background task.
pub struct UpdateChecker {
    /// Check interval.
    interval: Duration,
    /// Current state (shared with external readers).
    state: Arc<RwLock<UpdateState>>,
    /// Channel to trigger an update application (for future web API use).
    #[allow(dead_code)]
    apply_tx: watch::Sender<bool>,
    /// Channel to receive apply trigger.
    apply_rx: watch::Receiver<bool>,
    /// Path to the repository root.
    repo_root: String,
}

impl UpdateChecker {
    /// Create a new update checker.
    pub fn new(interval: Duration) -> Self {
        let (apply_tx, apply_rx) = watch::channel(false);
        Self {
            interval,
            state: Arc::new(RwLock::new(UpdateState::default())),
            apply_tx,
            apply_rx,
            repo_root: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string()),
        }
    }

    /// Get a clone of the shared state handle.
    pub fn state(&self) -> Arc<RwLock<UpdateState>> {
        Arc::clone(&self.state)
    }

    /// Trigger update application (called externally via web API).
    #[allow(dead_code)]
    pub fn trigger_apply(&self) {
        let _ = self.apply_tx.send(true);
    }

    /// Check for updates by fetching from origin/main.
    pub async fn check(&self) -> Result<UpdateState, String> {
        // Get current HEAD
        let current = self.get_current_commit().await?;

        // Fetch origin/main
        self.fetch_origin().await?;

        // Get origin/main HEAD
        let target = self.get_origin_main_commit().await?;

        // Check if we're behind
        if current == target {
            return Ok(UpdateState {
                update_available: false,
                current_commit: Some(current),
                target_commit: Some(target),
                commits_behind: 0,
                scope: RebuildScope::default(),
                last_error: None,
                applying: false,
            });
        }

        // Count commits behind
        let commits_behind = self.count_commits_behind().await?;

        // Analyze changed files to determine scope
        let scope = self.analyze_scope().await?;

        Ok(UpdateState {
            update_available: true,
            current_commit: Some(current),
            target_commit: Some(target),
            commits_behind,
            scope,
            last_error: None,
            applying: false,
        })
    }

    /// Run the background check loop.
    pub async fn run_loop(&self, mut shutdown_rx: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(self.interval);
        let mut apply_rx = self.apply_rx.clone();

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.check().await {
                        Ok(state) => {
                            let mut current = self.state.write().await;
                            // Preserve applying flag if set
                            let applying = current.applying;
                            *current = state;
                            current.applying = applying;

                            if current.update_available {
                                info!(
                                    current = ?current.current_commit,
                                    target = ?current.target_commit,
                                    commits_behind = current.commits_behind,
                                    scope = ?current.scope,
                                    "update available"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "update check failed");
                            let mut current = self.state.write().await;
                            current.last_error = Some(e);
                        }
                    }
                }
                _ = apply_rx.changed() => {
                    if *apply_rx.borrow() {
                        let mut state = self.state.write().await;
                        state.applying = true;
                        info!("update apply triggered");
                        // The actual application is handled by the run_loop in main
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("update checker shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// Get the current HEAD commit (short hash).
    async fn get_current_commit(&self) -> Result<String, String> {
        let output = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(&self.repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to run git rev-parse: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Fetch from origin.
    async fn fetch_origin(&self) -> Result<(), String> {
        let output = Command::new("git")
            .args(["fetch", "origin", "main"])
            .current_dir(&self.repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to run git fetch: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }

    /// Get the origin/main commit (short hash).
    async fn get_origin_main_commit(&self) -> Result<String, String> {
        let output = Command::new("git")
            .args(["rev-parse", "--short", "origin/main"])
            .current_dir(&self.repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to run git rev-parse origin/main: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git rev-parse origin/main failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Count commits behind origin/main.
    async fn count_commits_behind(&self) -> Result<u32, String> {
        let output = Command::new("git")
            .args(["rev-list", "--count", "HEAD..origin/main"])
            .current_dir(&self.repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to count commits: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git rev-list failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|e| format!("failed to parse commit count: {e}"))
    }

    /// Analyze changed files to determine rebuild scope.
    async fn analyze_scope(&self) -> Result<RebuildScope, String> {
        let output = Command::new("git")
            .args(["diff", "--name-only", "HEAD..origin/main"])
            .current_dir(&self.repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to get diff: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<&str> = stdout
            .lines()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(determine_rebuild_scope(&files))
    }
}

/// Determine rebuild scope from a list of changed files.
///
/// Rules:
/// - `crates/supervisor/**`, `src/runtime/Dockerfile`, `Makefile` → Container scope
/// - `crates/**` (except supervisor) → Server scope
/// - `web/**` → Frontend scope
pub fn determine_rebuild_scope(files: &[&str]) -> RebuildScope {
    let mut scope = RebuildScope::default();

    for &file in files {
        // Container scope: supervisor, runtime Dockerfile, Makefile
        if file.starts_with("crates/supervisor/")
            || file == "src/runtime/Dockerfile"
            || file == "Makefile"
        {
            scope.container = true;
            // Supervisor is part of the workspace, so server also needs rebuild
            if file.starts_with("crates/supervisor/") {
                scope.server = true;
            }
        }
        // Server scope: any other crates/** changes
        else if file.starts_with("crates/") {
            scope.server = true;
        }
        // Frontend scope: web/** changes
        else if file.starts_with("web/") {
            scope.frontend = true;
        }
        // Cargo workspace files affect server build
        else if file == "Cargo.toml" || file == "Cargo.lock" {
            scope.server = true;
        }
    }

    scope
}

/// Write the update scope file used by the wrapper script.
pub fn write_scope_file(scope: &RebuildScope, data_dir: &str) -> std::io::Result<()> {
    let scope_path = format!("{}/.update-scope", data_dir);
    std::fs::write(&scope_path, scope.to_scope_string())?;
    debug!(path = %scope_path, scope = %scope.to_scope_string(), "wrote update scope file");
    Ok(())
}

/// Exit code indicating that an update is ready.
pub const EXIT_CODE_UPDATE: i32 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scope_server_changes() {
        let files = vec!["crates/app/src/main.rs", "crates/server/src/lib.rs"];
        let scope = determine_rebuild_scope(&files);
        assert!(scope.server);
        assert!(!scope.container);
        assert!(!scope.frontend);
    }

    #[test]
    fn test_scope_supervisor_changes() {
        let files = vec!["crates/supervisor/src/main.rs"];
        let scope = determine_rebuild_scope(&files);
        assert!(scope.server); // supervisor is part of workspace
        assert!(scope.container);
        assert!(!scope.frontend);
    }

    #[test]
    fn test_scope_dockerfile_changes() {
        let files = vec!["src/runtime/Dockerfile"];
        let scope = determine_rebuild_scope(&files);
        assert!(!scope.server);
        assert!(scope.container);
        assert!(!scope.frontend);
    }

    #[test]
    fn test_scope_makefile_changes() {
        let files = vec!["Makefile"];
        let scope = determine_rebuild_scope(&files);
        assert!(!scope.server);
        assert!(scope.container);
        assert!(!scope.frontend);
    }

    #[test]
    fn test_scope_frontend_changes() {
        let files = vec!["web/src/App.tsx", "web/package.json"];
        let scope = determine_rebuild_scope(&files);
        assert!(!scope.server);
        assert!(!scope.container);
        assert!(scope.frontend);
    }

    #[test]
    fn test_scope_mixed_changes() {
        let files = vec![
            "crates/app/src/main.rs",
            "crates/supervisor/src/main.rs",
            "web/src/App.tsx",
        ];
        let scope = determine_rebuild_scope(&files);
        assert!(scope.server);
        assert!(scope.container);
        assert!(scope.frontend);
    }

    #[test]
    fn test_scope_cargo_files() {
        let files = vec!["Cargo.toml", "Cargo.lock"];
        let scope = determine_rebuild_scope(&files);
        assert!(scope.server);
        assert!(!scope.container);
        assert!(!scope.frontend);
    }

    #[test]
    fn test_scope_no_rebuild_needed() {
        let files = vec!["README.md", "docs/design.md", ".gitignore"];
        let scope = determine_rebuild_scope(&files);
        assert!(!scope.server);
        assert!(!scope.container);
        assert!(!scope.frontend);
        assert!(!scope.needs_rebuild());
    }

    #[test]
    fn test_scope_string_roundtrip() {
        let scope = RebuildScope {
            server: true,
            container: true,
            frontend: false,
        };
        let s = scope.to_scope_string();
        assert_eq!(s, "server,container");

        let parsed = RebuildScope::from_scope_string(&s);
        assert_eq!(parsed, scope);
    }

    #[test]
    fn test_scope_string_empty() {
        let scope = RebuildScope::default();
        let s = scope.to_scope_string();
        assert_eq!(s, "");

        let parsed = RebuildScope::from_scope_string(&s);
        assert_eq!(parsed, scope);
    }
}
