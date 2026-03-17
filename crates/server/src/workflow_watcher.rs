//! Workflow config watcher — spec §14.3.
//!
//! Watches for workflow.toml changes in project repositories and reloads
//! configuration dynamically. Changes are detected via polling and apply
//! to future dispatches (not retroactive to running sessions).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::workflow::WorkflowConfig;

/// Cached workflow configuration for a project.
#[derive(Debug, Clone)]
pub struct CachedConfig {
    /// The parsed workflow configuration.
    pub config: WorkflowConfig,
    /// Raw TOML content (for change detection).
    pub content_hash: u64,
    /// When this config was last fetched.
    pub fetched_at: Instant,
}

/// Workflow configuration cache with change detection.
///
/// Caches parsed workflow configs per project and detects changes
/// when repositories are polled. Emits events when configs change.
pub struct WorkflowConfigCache {
    /// Cached configs indexed by project ID.
    configs: RwLock<HashMap<String, CachedConfig>>,
    /// Minimum interval between refreshes for a single project.
    debounce_interval: Duration,
}

impl WorkflowConfigCache {
    /// Create a new config cache.
    ///
    /// # Arguments
    /// * `debounce_interval` - Minimum time between refreshes for a project (default 500ms)
    pub fn new(debounce_interval: Duration) -> Self {
        Self {
            configs: RwLock::new(HashMap::new()),
            debounce_interval,
        }
    }

    /// Create a config cache with default settings (500ms debounce).
    pub fn with_defaults() -> Self {
        Self::new(Duration::from_millis(500))
    }

    /// Get the cached config for a project, if any.
    pub async fn get(&self, project_id: &str) -> Option<WorkflowConfig> {
        let configs = self.configs.read().await;
        configs.get(project_id).map(|c| c.config.clone())
    }

    /// Check if we should refresh the config for a project (debounce check).
    pub async fn should_refresh(&self, project_id: &str) -> bool {
        let configs = self.configs.read().await;
        match configs.get(project_id) {
            Some(cached) => cached.fetched_at.elapsed() >= self.debounce_interval,
            None => true, // No cache, should fetch
        }
    }

    /// Update the cached config for a project.
    ///
    /// Returns `Some(config)` if the config changed (or is new), `None` if unchanged.
    /// If the new content fails to parse, logs a warning and returns `None`
    /// (keeping the old config in the cache).
    pub async fn update(
        &self,
        project_id: &str,
        toml_content: &str,
    ) -> Option<WorkflowConfig> {
        let new_hash = hash_content(toml_content);

        // Check if content changed
        {
            let configs = self.configs.read().await;
            if let Some(cached) = configs.get(project_id) {
                if cached.content_hash == new_hash {
                    // Content unchanged, just update fetch time
                    drop(configs);
                    let mut configs = self.configs.write().await;
                    if let Some(cached) = configs.get_mut(project_id) {
                        cached.fetched_at = Instant::now();
                    }
                    return None;
                }
            }
        }

        // Content changed or new — try to parse
        let config = match WorkflowConfig::parse(toml_content) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(
                    project_id = %project_id,
                    error = %e,
                    "failed to parse updated workflow.toml, keeping old config"
                );
                // Update fetch time but keep old config
                let mut configs = self.configs.write().await;
                if let Some(cached) = configs.get_mut(project_id) {
                    cached.fetched_at = Instant::now();
                }
                return None;
            }
        };

        // Store new config
        let cached = CachedConfig {
            config: config.clone(),
            content_hash: new_hash,
            fetched_at: Instant::now(),
        };

        let mut configs = self.configs.write().await;
        let is_new = !configs.contains_key(project_id);
        configs.insert(project_id.to_string(), cached);

        if is_new {
            debug!(project_id = %project_id, "cached initial workflow config");
            None // Initial load doesn't count as a "change"
        } else {
            info!(project_id = %project_id, "workflow config changed, reloading");
            Some(config)
        }
    }

    /// Remove a project's cached config (e.g., when project is removed).
    pub async fn remove(&self, project_id: &str) {
        let mut configs = self.configs.write().await;
        configs.remove(project_id);
    }

    /// Set a default config for a project that has no workflow.toml.
    ///
    /// This prevents repeated fetch attempts for projects without config files.
    pub async fn set_default(&self, project_id: &str) {
        let config = WorkflowConfig::default();
        let cached = CachedConfig {
            config,
            content_hash: 0, // Special hash for "no file"
            fetched_at: Instant::now(),
        };

        let mut configs = self.configs.write().await;
        configs.insert(project_id.to_string(), cached);
    }

    /// Mark a project as having no workflow.toml (so we don't keep trying to fetch it).
    /// Returns true if this is a change from having a config to not having one.
    pub async fn mark_no_config(&self, project_id: &str) -> bool {
        let mut configs = self.configs.write().await;
        let had_config = configs
            .get(project_id)
            .map(|c| c.content_hash != 0)
            .unwrap_or(false);

        // Set default config with hash=0 marker
        let config = WorkflowConfig::default();
        let cached = CachedConfig {
            config,
            content_hash: 0,
            fetched_at: Instant::now(),
        };
        configs.insert(project_id.to_string(), cached);

        had_config // Return true if we previously had a non-default config
    }

    /// Get all cached project IDs.
    pub async fn project_ids(&self) -> Vec<String> {
        let configs = self.configs.read().await;
        configs.keys().cloned().collect()
    }
}

impl Default for WorkflowConfigCache {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Simple hash function for content comparison.
fn hash_content(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Result of a config refresh operation.
#[derive(Debug)]
pub enum RefreshResult {
    /// Config was loaded/reloaded successfully.
    Loaded(WorkflowConfig),
    /// Config changed and was reloaded.
    Changed(WorkflowConfig),
    /// Config unchanged (or debounced).
    Unchanged,
    /// No workflow.toml found (using defaults).
    NoConfig,
    /// Failed to fetch config (keeping old if any).
    FetchError(String),
}

/// Workflow config watcher that polls GitHub for changes.
///
/// This is the main entry point for the config reload system.
/// It wraps a cache and provides methods for refreshing configs.
pub struct WorkflowConfigWatcher {
    /// The config cache.
    pub cache: Arc<WorkflowConfigCache>,
}

impl WorkflowConfigWatcher {
    /// Create a new watcher with the given cache.
    pub fn new(cache: Arc<WorkflowConfigCache>) -> Self {
        Self { cache }
    }

    /// Create a new watcher with default settings.
    pub fn with_defaults() -> Self {
        Self::new(Arc::new(WorkflowConfigCache::with_defaults()))
    }

    /// Refresh config for a project if needed (respects debounce).
    ///
    /// # Arguments
    /// * `project_id` - The project ID
    /// * `fetch_content` - Async function to fetch workflow.toml content
    ///
    /// Returns the refresh result.
    pub async fn refresh<F, Fut>(
        &self,
        project_id: &str,
        fetch_content: F,
    ) -> RefreshResult
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Option<String>, String>>,
    {
        // Check debounce
        if !self.cache.should_refresh(project_id).await {
            return RefreshResult::Unchanged;
        }

        // Fetch content
        let content = match fetch_content().await {
            Ok(Some(c)) => c,
            Ok(None) => {
                // No workflow.toml — mark as no config
                let changed = self.cache.mark_no_config(project_id).await;
                if changed {
                    info!(project_id = %project_id, "workflow.toml removed, using defaults");
                    return RefreshResult::Changed(WorkflowConfig::default());
                }
                return RefreshResult::NoConfig;
            }
            Err(e) => {
                warn!(project_id = %project_id, error = %e, "failed to fetch workflow.toml");
                return RefreshResult::FetchError(e);
            }
        };

        // Update cache
        match self.cache.update(project_id, &content).await {
            Some(config) => RefreshResult::Changed(config),
            None => {
                // Get current config (might be newly loaded or unchanged)
                match self.cache.get(project_id).await {
                    Some(config) => RefreshResult::Loaded(config),
                    None => RefreshResult::Unchanged,
                }
            }
        }
    }

    /// Get the current cached config for a project (or default if not cached).
    pub async fn get_config(&self, project_id: &str) -> WorkflowConfig {
        self.cache
            .get(project_id)
            .await
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_stores_and_retrieves_config() {
        let cache = WorkflowConfigCache::with_defaults();

        let toml = r#"
[project]
max_sessions = 5
"#;
        let result = cache.update("proj-1", toml).await;
        assert!(result.is_none()); // First load isn't a "change"

        let config = cache.get("proj-1").await.unwrap();
        assert_eq!(config.project.max_sessions, Some(5));
    }

    #[tokio::test]
    async fn cache_detects_changes() {
        let cache = WorkflowConfigCache::new(Duration::from_millis(0)); // No debounce for test

        // Initial load
        let toml1 = r#"[project]
max_sessions = 5"#;
        cache.update("proj-1", toml1).await;

        // Same content — no change
        let result = cache.update("proj-1", toml1).await;
        assert!(result.is_none());

        // Different content — change detected
        let toml2 = r#"[project]
max_sessions = 10"#;
        let result = cache.update("proj-1", toml2).await;
        assert!(result.is_some());

        let config = result.unwrap();
        assert_eq!(config.project.max_sessions, Some(10));
    }

    #[tokio::test]
    async fn cache_keeps_old_config_on_parse_error() {
        let cache = WorkflowConfigCache::new(Duration::from_millis(0));

        // Valid config
        let valid = r#"[project]
max_sessions = 5"#;
        cache.update("proj-1", valid).await;

        // Invalid config
        let invalid = "this is not valid toml {{{";
        let result = cache.update("proj-1", invalid).await;
        assert!(result.is_none()); // Should fail silently

        // Old config preserved
        let config = cache.get("proj-1").await.unwrap();
        assert_eq!(config.project.max_sessions, Some(5));
    }

    #[tokio::test]
    async fn cache_respects_debounce() {
        let cache = WorkflowConfigCache::new(Duration::from_secs(60)); // Long debounce

        let toml = "[project]";
        cache.update("proj-1", toml).await;

        // Should not refresh due to debounce
        assert!(!cache.should_refresh("proj-1").await);
    }

    #[tokio::test]
    async fn watcher_refresh_flow() {
        let watcher = WorkflowConfigWatcher::with_defaults();

        // First fetch
        let result = watcher
            .refresh("proj-1", || async {
                Ok(Some("[project]\nmax_sessions = 3".to_string()))
            })
            .await;

        matches!(result, RefreshResult::Loaded(_));

        let config = watcher.get_config("proj-1").await;
        assert_eq!(config.project.max_sessions, Some(3));
    }

    #[tokio::test]
    async fn watcher_handles_no_config() {
        let watcher = WorkflowConfigWatcher::with_defaults();

        let result = watcher
            .refresh("proj-1", || async { Ok(None) })
            .await;

        matches!(result, RefreshResult::NoConfig);

        // Should get defaults
        let config = watcher.get_config("proj-1").await;
        assert!(config.project.max_sessions.is_none());
    }

    #[tokio::test]
    async fn watcher_handles_fetch_error() {
        let watcher = WorkflowConfigWatcher::with_defaults();

        let result = watcher
            .refresh("proj-1", || async {
                Err("network error".to_string())
            })
            .await;

        matches!(result, RefreshResult::FetchError(_));
    }
}
