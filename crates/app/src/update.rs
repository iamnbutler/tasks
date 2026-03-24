//! Self-update infrastructure — detects updates and triggers restarts.
//!
//! The update mechanism works in two parts:
//! 1. UpdateChecker — background task that checks for git updates
//! 2. UpdateExecutor — handles clean shutdown and exit with code 100
//!
//! The wrapper script (scripts/tasks-runner.sh) handles the actual
//! pull, rebuild, and restart when it sees exit code 100.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Exit code indicating the server should be restarted after update.
pub const UPDATE_EXIT_CODE: i32 = 100;

/// What needs to be rebuilt after pulling updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildScope {
    /// No rebuild needed (only documentation or non-code changes).
    None,
    /// Only the web frontend needs rebuilding.
    Frontend,
    /// Only the server needs rebuilding.
    Server,
    /// Only the container image needs rebuilding.
    Container,
    /// Server + frontend need rebuilding.
    ServerAndFrontend,
    /// Server + container image need rebuilding.
    ServerAndContainer,
    /// Everything needs rebuilding (server, frontend, container).
    All,
}

impl RebuildScope {
    /// Parse a scope from a string (for reading from .update-scope file).
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "none" => Self::None,
            "frontend" => Self::Frontend,
            "server" => Self::Server,
            "container" => Self::Container,
            "server_and_frontend" => Self::ServerAndFrontend,
            "server_and_container" => Self::ServerAndContainer,
            "all" => Self::All,
            _ => Self::All, // default to full rebuild on unknown
        }
    }

    /// Convert to string (for writing to .update-scope file).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Frontend => "frontend",
            Self::Server => "server",
            Self::Container => "container",
            Self::ServerAndFrontend => "server_and_frontend",
            Self::ServerAndContainer => "server_and_container",
            Self::All => "all",
        }
    }

    /// Merge two scopes, returning the broader scope.
    pub fn merge(&self, other: &Self) -> Self {
        use RebuildScope::*;
        match (self, other) {
            // None + anything = other
            (None, x) | (x, None) => x.clone(),
            // Same = same
            (Frontend, Frontend) => Frontend,
            (Server, Server) => Server,
            (Container, Container) => Container,
            (ServerAndFrontend, ServerAndFrontend) => ServerAndFrontend,
            (ServerAndContainer, ServerAndContainer) => ServerAndContainer,
            (All, _) | (_, All) => All,
            // Server + Frontend = ServerAndFrontend
            (Server, Frontend) | (Frontend, Server) => ServerAndFrontend,
            // Server + Container = ServerAndContainer
            (Server, Container) | (Container, Server) => ServerAndContainer,
            // Frontend + Container = All (need both + server is implicit)
            (Frontend, Container) | (Container, Frontend) => All,
            // ServerAndFrontend + Container = All
            (ServerAndFrontend, Container) | (Container, ServerAndFrontend) => All,
            // ServerAndContainer + Frontend = All
            (ServerAndContainer, Frontend) | (Frontend, ServerAndContainer) => All,
            // ServerAndFrontend + Server = ServerAndFrontend
            (ServerAndFrontend, Server) | (Server, ServerAndFrontend) => ServerAndFrontend,
            // ServerAndContainer + Server = ServerAndContainer
            (ServerAndContainer, Server) | (Server, ServerAndContainer) => ServerAndContainer,
            // ServerAndFrontend + ServerAndContainer = All
            (ServerAndFrontend, ServerAndContainer) | (ServerAndContainer, ServerAndFrontend) => {
                All
            }
            // Subset + superset = superset (no escalation needed)
            (Frontend, ServerAndFrontend) | (ServerAndFrontend, Frontend) => ServerAndFrontend,
            (Container, ServerAndContainer) | (ServerAndContainer, Container) => ServerAndContainer,
        }
    }
}

/// State of an available update.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields used for future API endpoints and logging
pub struct UpdateInfo {
    /// The commit SHA we're currently at.
    pub current_commit: String,
    /// The commit SHA available on origin/main.
    pub available_commit: String,
    /// Number of commits behind.
    pub commits_behind: u32,
    /// What needs to be rebuilt.
    pub scope: RebuildScope,
    /// Changed files (for debugging/logging).
    pub changed_files: Vec<String>,
}

/// Shared update state.
pub struct UpdateState {
    /// Whether an update is available.
    update_available: RwLock<Option<UpdateInfo>>,
    /// Whether an update is currently being applied.
    applying: AtomicBool,
}

impl UpdateState {
    pub fn new() -> Self {
        Self {
            update_available: RwLock::new(None),
            applying: AtomicBool::new(false),
        }
    }

    /// Check if an update is available.
    pub async fn is_available(&self) -> bool {
        self.update_available.read().await.is_some()
    }

    /// Get update info if available.
    pub async fn get_info(&self) -> Option<UpdateInfo> {
        self.update_available.read().await.clone()
    }

    /// Set update info.
    pub async fn set_info(&self, info: Option<UpdateInfo>) {
        *self.update_available.write().await = info;
    }

    /// Mark that we're applying an update.
    pub fn set_applying(&self, applying: bool) {
        self.applying.store(applying, Ordering::SeqCst);
    }

    /// Check if we're currently applying an update.
    pub fn is_applying(&self) -> bool {
        self.applying.load(Ordering::SeqCst)
    }
}

impl Default for UpdateState {
    fn default() -> Self {
        Self::new()
    }
}

/// Determines rebuild scope from a list of changed files.
///
/// Rules:
/// - `crates/supervisor/**`, `src/runtime/Dockerfile`, `Makefile` -> Container
/// - `crates/**` (excluding supervisor) -> Server
/// - `web/**` -> Frontend
pub fn determine_rebuild_scope(changed_files: &[String]) -> RebuildScope {
    let mut scope = RebuildScope::None;

    for file in changed_files {
        let file_scope = classify_file(file);
        scope = scope.merge(&file_scope);

        // Early exit if we've already determined we need everything
        if scope == RebuildScope::All {
            break;
        }
    }

    scope
}

/// Classify a single file path into its rebuild scope.
fn classify_file(path: &str) -> RebuildScope {
    // Container-related files
    if path.starts_with("crates/supervisor/")
        || path == "src/runtime/Dockerfile"
        || path == "Makefile"
        || (path.starts_with("scripts/") && path.contains("container"))
    {
        return RebuildScope::Container;
    }

    // Server-related files (Rust crates except supervisor)
    if path.starts_with("crates/") {
        return RebuildScope::Server;
    }

    // Frontend-related files
    if path.starts_with("web/") {
        return RebuildScope::Frontend;
    }

    // Cargo.toml at root affects server build
    if path == "Cargo.toml" || path == "Cargo.lock" {
        return RebuildScope::Server;
    }

    // Everything else (docs, config, etc.) doesn't require rebuild
    RebuildScope::None
}

/// Check for updates by running git commands.
///
/// Returns `Ok(Some(UpdateInfo))` if updates are available,
/// `Ok(None)` if we're up to date, or an error if git commands failed.
pub async fn check_for_updates(repo_path: &Path) -> Result<Option<UpdateInfo>, String> {
    // Run git fetch to get latest from origin
    let fetch_output = Command::new("git")
        .args(["fetch", "origin", "main"])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run git fetch: {e}"))?;

    if !fetch_output.status.success() {
        let stderr = String::from_utf8_lossy(&fetch_output.stderr);
        return Err(format!("git fetch failed: {stderr}"));
    }

    // Get current HEAD commit
    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| format!("Failed to get HEAD: {e}"))?;

    let current_commit = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();

    // Get origin/main commit
    let origin_output = Command::new("git")
        .args(["rev-parse", "origin/main"])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| format!("Failed to get origin/main: {e}"))?;

    let available_commit = String::from_utf8_lossy(&origin_output.stdout)
        .trim()
        .to_string();

    // If commits are the same, no update available
    if current_commit == available_commit {
        debug!("no updates available (at {current_commit})");
        return Ok(None);
    }

    // Count commits behind
    let count_output = Command::new("git")
        .args([
            "rev-list",
            "--count",
            &format!("{current_commit}..origin/main"),
        ])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| format!("Failed to count commits: {e}"))?;

    let commits_behind: u32 = String::from_utf8_lossy(&count_output.stdout)
        .trim()
        .parse()
        .unwrap_or(1);

    // Get changed files
    let diff_output = Command::new("git")
        .args(["diff", "--name-only", "HEAD..origin/main"])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| format!("Failed to get diff: {e}"))?;

    let diff_text = String::from_utf8_lossy(&diff_output.stdout);
    let changed_files: Vec<String> = diff_text.lines().map(|s| s.to_string()).collect();

    // Determine rebuild scope
    let scope = determine_rebuild_scope(&changed_files);

    info!(
        current = %current_commit,
        available = %available_commit,
        commits_behind = commits_behind,
        scope = ?scope,
        changed_files = changed_files.len(),
        "update available"
    );

    Ok(Some(UpdateInfo {
        current_commit,
        available_commit,
        commits_behind,
        scope,
        changed_files,
    }))
}

/// Write the update scope to a file for the wrapper script to read.
pub fn write_update_scope(data_dir: &str, scope: &RebuildScope) -> Result<(), String> {
    let scope_file = format!("{data_dir}/.update-scope");
    std::fs::write(&scope_file, scope.as_str())
        .map_err(|e| format!("Failed to write scope file: {e}"))?;
    info!(scope = ?scope, file = %scope_file, "wrote update scope");
    Ok(())
}

/// Background update checker task.
///
/// Periodically checks for git updates and updates the shared state.
pub async fn update_checker_loop(
    state: Arc<UpdateState>,
    repo_path: std::path::PathBuf,
    check_interval: Duration,
) {
    let mut interval = tokio::time::interval(check_interval);

    loop {
        interval.tick().await;

        // Don't check if we're already applying an update
        if state.is_applying() {
            continue;
        }

        match check_for_updates(&repo_path).await {
            Ok(Some(info)) => {
                state.set_info(Some(info)).await;
            }
            Ok(None) => {
                state.set_info(None).await;
            }
            Err(e) => {
                warn!(error = %e, "update check failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_file_server() {
        assert_eq!(classify_file("crates/app/src/main.rs"), RebuildScope::Server);
        assert_eq!(
            classify_file("crates/server/src/lib.rs"),
            RebuildScope::Server
        );
        assert_eq!(classify_file("Cargo.toml"), RebuildScope::Server);
        assert_eq!(classify_file("Cargo.lock"), RebuildScope::Server);
    }

    #[test]
    fn test_classify_file_container() {
        assert_eq!(
            classify_file("crates/supervisor/src/main.rs"),
            RebuildScope::Container
        );
        assert_eq!(
            classify_file("src/runtime/Dockerfile"),
            RebuildScope::Container
        );
        assert_eq!(classify_file("Makefile"), RebuildScope::Container);
    }

    #[test]
    fn test_classify_file_frontend() {
        assert_eq!(
            classify_file("web/src/App.tsx"),
            RebuildScope::Frontend
        );
        assert_eq!(
            classify_file("web/package.json"),
            RebuildScope::Frontend
        );
    }

    #[test]
    fn test_classify_file_none() {
        assert_eq!(classify_file("README.md"), RebuildScope::None);
        assert_eq!(classify_file("docs/design/foo.md"), RebuildScope::None);
        assert_eq!(classify_file(".gitignore"), RebuildScope::None);
    }

    #[test]
    fn test_determine_rebuild_scope_server_only() {
        let files = vec![
            "crates/app/src/main.rs".to_string(),
            "crates/server/src/lib.rs".to_string(),
        ];
        assert_eq!(determine_rebuild_scope(&files), RebuildScope::Server);
    }

    #[test]
    fn test_determine_rebuild_scope_frontend_only() {
        let files = vec![
            "web/src/App.tsx".to_string(),
            "web/package.json".to_string(),
        ];
        assert_eq!(determine_rebuild_scope(&files), RebuildScope::Frontend);
    }

    #[test]
    fn test_determine_rebuild_scope_container_only() {
        let files = vec![
            "crates/supervisor/src/main.rs".to_string(),
            "Makefile".to_string(),
        ];
        assert_eq!(determine_rebuild_scope(&files), RebuildScope::Container);
    }

    #[test]
    fn test_determine_rebuild_scope_server_and_frontend() {
        let files = vec![
            "crates/app/src/main.rs".to_string(),
            "web/src/App.tsx".to_string(),
        ];
        assert_eq!(
            determine_rebuild_scope(&files),
            RebuildScope::ServerAndFrontend
        );
    }

    #[test]
    fn test_determine_rebuild_scope_server_and_container() {
        let files = vec![
            "crates/app/src/main.rs".to_string(),
            "crates/supervisor/src/main.rs".to_string(),
        ];
        assert_eq!(
            determine_rebuild_scope(&files),
            RebuildScope::ServerAndContainer
        );
    }

    #[test]
    fn test_determine_rebuild_scope_all() {
        let files = vec![
            "crates/app/src/main.rs".to_string(),
            "web/src/App.tsx".to_string(),
            "crates/supervisor/src/main.rs".to_string(),
        ];
        assert_eq!(determine_rebuild_scope(&files), RebuildScope::All);
    }

    #[test]
    fn test_determine_rebuild_scope_docs_only() {
        let files = vec![
            "README.md".to_string(),
            "docs/design/foo.md".to_string(),
        ];
        assert_eq!(determine_rebuild_scope(&files), RebuildScope::None);
    }

    #[test]
    fn test_scope_merge() {
        assert_eq!(
            RebuildScope::None.merge(&RebuildScope::Server),
            RebuildScope::Server
        );
        assert_eq!(
            RebuildScope::Server.merge(&RebuildScope::Frontend),
            RebuildScope::ServerAndFrontend
        );
        assert_eq!(
            RebuildScope::Server.merge(&RebuildScope::Container),
            RebuildScope::ServerAndContainer
        );
        assert_eq!(
            RebuildScope::ServerAndFrontend.merge(&RebuildScope::Container),
            RebuildScope::All
        );
    }

    #[test]
    fn test_scope_roundtrip() {
        let scopes = [
            RebuildScope::None,
            RebuildScope::Frontend,
            RebuildScope::Server,
            RebuildScope::Container,
            RebuildScope::ServerAndFrontend,
            RebuildScope::ServerAndContainer,
            RebuildScope::All,
        ];

        for scope in &scopes {
            let s = scope.as_str();
            let parsed = RebuildScope::from_str(s);
            assert_eq!(&parsed, scope, "roundtrip failed for {s}");
        }
    }
}
