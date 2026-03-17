//! Workspace cleanup — spec Section 10.3.
//!
//! Manages workspace lifecycle: cleanup triggers, stale detection,
//! and cleanup execution. Event logs are retained independently.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;

use events::{Actor, Event, EventBus, EventType};
use models::workspace::{Workspace, WorkspaceStatus};
use runtime::ContainerRuntime;

/// Default idle threshold for stale workspace detection (7 days).
pub const DEFAULT_IDLE_THRESHOLD_DAYS: i64 = 7;

#[derive(Debug, Error)]
pub enum WorkspaceCleanupError {
    #[error("workspace not found: {0}")]
    NotFound(String),
    #[error("container error: {0}")]
    Container(String),
    #[error("event store error: {0}")]
    EventStore(#[from] events::StoreError),
}

/// Cleanup trigger reason — why a workspace is being cleaned up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupReason {
    /// Task completed successfully.
    TaskCompleted,
    /// Task was cancelled.
    TaskCancelled,
    /// Associated PR was merged.
    PrMerged,
    /// Workspace was idle for too long.
    Stale,
    /// Manual cleanup request.
    Manual,
}

impl CleanupReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskCompleted => "task_completed",
            Self::TaskCancelled => "task_cancelled",
            Self::PrMerged => "pr_merged",
            Self::Stale => "stale",
            Self::Manual => "manual",
        }
    }
}

/// Workspace cleanup manager — handles cleanup lifecycle.
///
/// The cleanup process (spec §10.3):
/// 1. Stop any running container
/// 2. Mark workspace as cleaned
/// 3. Emit cleanup event
///
/// Event logs (JSONL files) are retained independently — deleting a
/// workspace does not delete session history (spec §10.3).
pub struct WorkspaceCleanupManager<R: ContainerRuntime> {
    runtime: Arc<R>,
    event_bus: Arc<EventBus>,
    workspaces: Arc<RwLock<Vec<Workspace>>>,
    /// Idle threshold for stale detection.
    idle_threshold: chrono::Duration,
}

impl<R: ContainerRuntime + Send + Sync + 'static> WorkspaceCleanupManager<R> {
    /// Create a new cleanup manager.
    pub fn new(runtime: Arc<R>, event_bus: Arc<EventBus>) -> Self {
        Self {
            runtime,
            event_bus,
            workspaces: Arc::new(RwLock::new(Vec::new())),
            idle_threshold: chrono::Duration::days(DEFAULT_IDLE_THRESHOLD_DAYS),
        }
    }

    /// Set the idle threshold for stale detection.
    pub fn with_idle_threshold(mut self, days: i64) -> Self {
        self.idle_threshold = chrono::Duration::days(days);
        self
    }

    /// Register a new workspace.
    pub async fn register(&self, workspace: Workspace) {
        let mut workspaces = self.workspaces.write().await;
        // Remove any existing workspace with the same ID
        workspaces.retain(|ws| ws.id != workspace.id);
        workspaces.push(workspace);
    }

    /// Get a workspace by ID.
    pub async fn get(&self, workspace_id: &str) -> Option<Workspace> {
        let workspaces = self.workspaces.read().await;
        workspaces.iter().find(|ws| ws.id == workspace_id).cloned()
    }

    /// Get a workspace by task ID.
    pub async fn get_by_task(&self, task_id: &str) -> Option<Workspace> {
        let workspaces = self.workspaces.read().await;
        workspaces.iter().find(|ws| ws.task_id == task_id).cloned()
    }

    /// Update last activity timestamp for a workspace.
    pub async fn touch(&self, workspace_id: &str) {
        let mut workspaces = self.workspaces.write().await;
        if let Some(ws) = workspaces.iter_mut().find(|ws| ws.id == workspace_id) {
            ws.touch();
        }
    }

    /// Mark a workspace as idle (no active session).
    pub async fn mark_idle(&self, workspace_id: &str) {
        let mut workspaces = self.workspaces.write().await;
        if let Some(ws) = workspaces.iter_mut().find(|ws| ws.id == workspace_id) {
            ws.mark_idle();
        }
    }

    /// Schedule a workspace for cleanup.
    ///
    /// This is triggered by:
    /// - Task completing (state → Completed)
    /// - Task cancelled (state → Cancelled)
    /// - PR merged (MergeStatus → Merged)
    pub async fn schedule_cleanup(
        &self,
        workspace_id: &str,
        reason: CleanupReason,
    ) -> Result<(), WorkspaceCleanupError> {
        let workspace = {
            let mut workspaces = self.workspaces.write().await;
            let ws = workspaces
                .iter_mut()
                .find(|ws| ws.id == workspace_id)
                .ok_or_else(|| WorkspaceCleanupError::NotFound(workspace_id.to_string()))?;
            ws.schedule_cleanup();
            ws.clone()
        };

        tracing::info!(
            workspace_id = %workspace_id,
            task_id = %workspace.task_id,
            reason = %reason.as_str(),
            "workspace scheduled for cleanup"
        );

        // Emit scheduled event
        let event = Event::new(
            EventType::WorkspaceCleanupScheduled,
            &workspace.task_id,
            Actor::System,
            serde_json::json!({
                "workspace_id": workspace_id,
                "reason": reason.as_str(),
            }),
        );
        self.event_bus.publish(event).await?;

        Ok(())
    }

    /// Execute cleanup for a workspace.
    ///
    /// Cleanup process (spec §10.3):
    /// 1. Stop any running container
    /// 2. Mark workspace as cleaned
    /// 3. Emit cleanup completed event
    ///
    /// Event logs are retained independently.
    pub async fn execute_cleanup(
        &self,
        workspace_id: &str,
        reason: CleanupReason,
    ) -> Result<(), WorkspaceCleanupError> {
        let workspace = self
            .get(workspace_id)
            .await
            .ok_or_else(|| WorkspaceCleanupError::NotFound(workspace_id.to_string()))?;

        tracing::info!(
            workspace_id = %workspace_id,
            task_id = %workspace.task_id,
            container_id = ?workspace.container_id,
            reason = %reason.as_str(),
            "executing workspace cleanup"
        );

        // Step 1: Stop and destroy any running container
        if let Some(ref container_id) = workspace.container_id {
            if let Err(e) = self.runtime.destroy(container_id).await {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    container_id = %container_id,
                    error = %e,
                    "failed to destroy container during cleanup (may already be stopped)"
                );
                // Continue with cleanup even if container destroy fails
            }
        }

        // Step 2: Mark workspace as cleaned
        {
            let mut workspaces = self.workspaces.write().await;
            if let Some(ws) = workspaces.iter_mut().find(|ws| ws.id == workspace_id) {
                ws.mark_cleaned();
            }
        }

        // Step 3: Emit cleanup completed event
        let event = Event::new(
            EventType::WorkspaceCleanupCompleted,
            &workspace.task_id,
            Actor::System,
            serde_json::json!({
                "workspace_id": workspace_id,
                "reason": reason.as_str(),
                "container_destroyed": workspace.container_id.is_some(),
            }),
        );
        self.event_bus.publish(event).await?;

        tracing::info!(
            workspace_id = %workspace_id,
            task_id = %workspace.task_id,
            "workspace cleanup completed"
        );

        Ok(())
    }

    /// Find and cleanup stale workspaces (spec §10.3).
    ///
    /// Workspaces with no activity for longer than the idle threshold
    /// are eligible for cleanup.
    pub async fn cleanup_stale(&self) -> Result<Vec<String>, WorkspaceCleanupError> {
        let stale_ids: Vec<String> = {
            let workspaces = self.workspaces.read().await;
            workspaces
                .iter()
                .filter(|ws| ws.is_stale(self.idle_threshold))
                .map(|ws| ws.id.clone())
                .collect()
        };

        let mut cleaned = Vec::new();
        for workspace_id in stale_ids {
            match self.execute_cleanup(&workspace_id, CleanupReason::Stale).await {
                Ok(()) => cleaned.push(workspace_id),
                Err(e) => {
                    tracing::error!(
                        workspace_id = %workspace_id,
                        error = %e,
                        "failed to cleanup stale workspace"
                    );
                }
            }
        }

        Ok(cleaned)
    }

    /// Remove cleaned workspaces from tracking.
    pub async fn prune_cleaned(&self) {
        let mut workspaces = self.workspaces.write().await;
        workspaces.retain(|ws| ws.status != WorkspaceStatus::Cleaned);
    }

    /// Get all workspaces (for debugging/monitoring).
    pub async fn list(&self) -> Vec<Workspace> {
        self.workspaces.read().await.clone()
    }

    /// Get count of active workspaces.
    pub async fn active_count(&self) -> usize {
        let workspaces = self.workspaces.read().await;
        workspaces
            .iter()
            .filter(|ws| matches!(ws.status, WorkspaceStatus::Active | WorkspaceStatus::Idle))
            .count()
    }
}

/// Trigger cleanup for a task's workspace when task reaches a terminal state.
///
/// Called by the server when a task transitions to Completed or Cancelled.
pub async fn trigger_cleanup_on_task_terminal<R: ContainerRuntime + Send + Sync + 'static>(
    cleanup_manager: &WorkspaceCleanupManager<R>,
    task_id: &str,
    is_completed: bool,
) -> Result<(), WorkspaceCleanupError> {
    let workspace = match cleanup_manager.get_by_task(task_id).await {
        Some(ws) => ws,
        None => {
            tracing::debug!(task_id = %task_id, "no workspace to cleanup for task");
            return Ok(());
        }
    };

    let reason = if is_completed {
        CleanupReason::TaskCompleted
    } else {
        CleanupReason::TaskCancelled
    };

    cleanup_manager.execute_cleanup(&workspace.id, reason).await
}

/// Trigger cleanup when a PR is merged.
///
/// Called by the server when a merge queue entry transitions to Merged.
pub async fn trigger_cleanup_on_merge<R: ContainerRuntime + Send + Sync + 'static>(
    cleanup_manager: &WorkspaceCleanupManager<R>,
    task_id: &str,
) -> Result<(), WorkspaceCleanupError> {
    let workspace = match cleanup_manager.get_by_task(task_id).await {
        Some(ws) => ws,
        None => {
            tracing::debug!(task_id = %task_id, "no workspace to cleanup for merged PR");
            return Ok(());
        }
    };

    cleanup_manager
        .execute_cleanup(&workspace.id, CleanupReason::PrMerged)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use chrono::Utc;

    /// Mock container runtime for testing.
    struct MockRuntime {
        destroy_count: AtomicUsize,
    }

    impl MockRuntime {
        fn new() -> Self {
            Self {
                destroy_count: AtomicUsize::new(0),
            }
        }
    }

    impl ContainerRuntime for MockRuntime {
        async fn create(
            &self,
            _config: &runtime::ContainerConfig,
        ) -> Result<String, runtime::ContainerError> {
            Ok("mock-container".to_string())
        }

        async fn start(
            &self,
            _container_id: &str,
        ) -> Result<runtime::StdioTransport, runtime::ContainerError> {
            unimplemented!()
        }

        async fn stop(&self, _container_id: &str) -> Result<(), runtime::ContainerError> {
            Ok(())
        }

        async fn destroy(&self, _container_id: &str) -> Result<(), runtime::ContainerError> {
            self.destroy_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn container_exists(&self, _container_id: &str) -> Result<bool, runtime::ContainerError> {
            Ok(true)
        }
    }

    async fn test_event_bus() -> Arc<EventBus> {
        let dir = tempfile::tempdir().unwrap();
        let store = events::EventStore::new(dir.path());
        Arc::new(events::EventBus::new(store, 64))
    }

    #[tokio::test]
    async fn register_and_get_workspace() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime, bus);

        let ws = Workspace::new("ws-1", "task-1");
        manager.register(ws).await;

        let retrieved = manager.get("ws-1").await.unwrap();
        assert_eq!(retrieved.id, "ws-1");
        assert_eq!(retrieved.task_id, "task-1");
    }

    #[tokio::test]
    async fn get_by_task_finds_workspace() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime, bus);

        let ws = Workspace::new("ws-1", "task-1");
        manager.register(ws).await;

        let retrieved = manager.get_by_task("task-1").await.unwrap();
        assert_eq!(retrieved.id, "ws-1");
    }

    #[tokio::test]
    async fn execute_cleanup_destroys_container() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime.clone(), bus);

        let mut ws = Workspace::new("ws-1", "task-1");
        ws.container_id = Some("container-123".to_string());
        manager.register(ws).await;

        manager
            .execute_cleanup("ws-1", CleanupReason::TaskCompleted)
            .await
            .unwrap();

        assert_eq!(runtime.destroy_count.load(Ordering::SeqCst), 1);

        let cleaned = manager.get("ws-1").await.unwrap();
        assert_eq!(cleaned.status, WorkspaceStatus::Cleaned);
        assert!(cleaned.container_id.is_none());
    }

    #[tokio::test]
    async fn cleanup_stale_workspaces() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime, bus).with_idle_threshold(7);

        // Create a stale workspace
        let mut stale = Workspace::new("ws-stale", "task-stale");
        stale.last_activity_at = Utc::now() - chrono::Duration::days(10);
        stale.status = WorkspaceStatus::Idle;
        manager.register(stale).await;

        // Create a fresh workspace
        let fresh = Workspace::new("ws-fresh", "task-fresh");
        manager.register(fresh).await;

        let cleaned = manager.cleanup_stale().await.unwrap();

        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0], "ws-stale");

        let stale_ws = manager.get("ws-stale").await.unwrap();
        assert_eq!(stale_ws.status, WorkspaceStatus::Cleaned);

        let fresh_ws = manager.get("ws-fresh").await.unwrap();
        assert_eq!(fresh_ws.status, WorkspaceStatus::Active);
    }

    #[tokio::test]
    async fn trigger_cleanup_on_task_completed() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime.clone(), bus);

        let mut ws = Workspace::new("ws-1", "task-1");
        ws.container_id = Some("container-123".to_string());
        manager.register(ws).await;

        trigger_cleanup_on_task_terminal(&manager, "task-1", true)
            .await
            .unwrap();

        let cleaned = manager.get("ws-1").await.unwrap();
        assert_eq!(cleaned.status, WorkspaceStatus::Cleaned);
    }

    #[tokio::test]
    async fn test_trigger_cleanup_on_merge() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime.clone(), bus);

        let ws = Workspace::new("ws-1", "task-1");
        manager.register(ws).await;

        super::trigger_cleanup_on_merge(&manager, "task-1").await.unwrap();

        let cleaned = manager.get("ws-1").await.unwrap();
        assert_eq!(cleaned.status, WorkspaceStatus::Cleaned);
    }

    #[tokio::test]
    async fn prune_removes_cleaned() {
        let runtime = Arc::new(MockRuntime::new());
        let bus = test_event_bus().await;
        let manager = WorkspaceCleanupManager::new(runtime, bus);

        let mut ws1 = Workspace::new("ws-1", "task-1");
        ws1.mark_cleaned();
        manager.register(ws1).await;

        let ws2 = Workspace::new("ws-2", "task-2");
        manager.register(ws2).await;

        manager.prune_cleaned().await;

        let all = manager.list().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "ws-2");
    }
}
