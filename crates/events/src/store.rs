//! Append-only event storage.
//!
//! Events are stored per-task as JSONL files: `<task-id>/events.jsonl`

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Event;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Append-only event store backed by the filesystem.
///
/// Events are stored per-task in JSONL format. Each task gets its own
/// directory with an `events.jsonl` file.
pub struct EventStore {
    root: PathBuf,
}

impl EventStore {
    /// Create a new event store at the given root directory.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Path to a task's event log file.
    fn task_log_path(&self, task_id: &str) -> PathBuf {
        self.root.join(task_id).join("events.jsonl")
    }

    /// Append an event to the store.
    ///
    /// Creates the task directory if it doesn't exist.
    pub async fn append(&self, event: &Event) -> Result<(), StoreError> {
        let path = self.task_log_path(&event.task);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;

        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;

        Ok(())
    }

    /// Read all events for a task.
    pub async fn read_task(&self, task_id: &str) -> Result<Vec<Event>, StoreError> {
        let path = self.task_log_path(task_id);

        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut events = Vec::new();

        while let Some(line) = lines.next_line().await? {
            if line.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)?;
            events.push(event);
        }

        Ok(events)
    }

    /// Read events for a task starting from a given offset.
    ///
    /// Returns events starting from `offset` (0-indexed).
    pub async fn read_task_from(
        &self,
        task_id: &str,
        offset: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let events = self.read_task(task_id).await?;
        Ok(events.into_iter().skip(offset).collect())
    }

    /// Query events across all tasks, filtered by an event-type prefix.
    ///
    /// Scans every task's event log and returns events whose type starts with
    /// `type_prefix` (e.g. `"orchestrator:"`).  Results are sorted by timestamp
    /// ascending and truncated to `limit`.
    pub async fn query_by_type_prefix(
        &self,
        type_prefix: &str,
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let task_ids = self.list_tasks().await?;
        let mut matching = Vec::new();

        for task_id in &task_ids {
            let events = self.read_task(task_id).await?;
            for event in events {
                if event.event_type.as_str().starts_with(type_prefix) {
                    matching.push(event);
                }
            }
        }

        // Also scan the empty-string task dir (system-wide events like orchestrator messages)
        let empty_events = self.read_task("").await?;
        for event in empty_events {
            if event.event_type.as_str().starts_with(type_prefix) {
                matching.push(event);
            }
        }

        // Sort by timestamp ascending
        matching.sort_by(|a, b| a.ts.cmp(&b.ts));

        // Apply limit (take from the end to get the most recent)
        if matching.len() > limit {
            matching = matching.split_off(matching.len() - limit);
        }

        Ok(matching)
    }

    /// List all task IDs that have event logs.
    pub async fn list_tasks(&self) -> Result<Vec<String>, StoreError> {
        let mut tasks = Vec::new();

        if !self.root.exists() {
            return Ok(tasks);
        }

        let mut entries = fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name() {
                    if let Some(s) = name.to_str() {
                        // Verify it has an events file
                        if path.join("events.jsonl").exists() {
                            tasks.push(s.to_string());
                        }
                    }
                }
            }
        }

        Ok(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Actor, EventType};
    use tempfile::tempdir;

    #[tokio::test]
    async fn append_and_read() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        let event = Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({"title": "Test task"}),
        );

        store.append(&event).await.unwrap();

        let events = store.read_task("task-1").await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event.id);
    }

    #[tokio::test]
    async fn read_nonexistent_task() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        let events = store.read_task("no-such-task").await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn list_tasks() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        store
            .append(&Event::new(
                EventType::TaskCreated,
                "task-a",
                Actor::System,
                serde_json::json!({}),
            ))
            .await
            .unwrap();

        store
            .append(&Event::new(
                EventType::TaskCreated,
                "task-b",
                Actor::System,
                serde_json::json!({}),
            ))
            .await
            .unwrap();

        let mut tasks = store.list_tasks().await.unwrap();
        tasks.sort();
        assert_eq!(tasks, vec!["task-a", "task-b"]);
    }

    #[tokio::test]
    async fn query_by_type_prefix() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        // Events across different task dirs
        store
            .append(&Event::new(
                EventType::OrchestratorDecision,
                "task-1",
                Actor::Orchestrator,
                serde_json::json!({"approved": true}),
            ))
            .await
            .unwrap();

        store
            .append(&Event::new(
                EventType::TaskCreated,
                "task-1",
                Actor::System,
                serde_json::json!({}),
            ))
            .await
            .unwrap();

        store
            .append(&Event::new(
                EventType::OrchestratorMessage,
                "", // system-wide
                Actor::Human,
                serde_json::json!({"message": "hello"}),
            ))
            .await
            .unwrap();

        store
            .append(&Event::new(
                EventType::OrchestratorFeedback,
                "task-2",
                Actor::Orchestrator,
                serde_json::json!({"feedback": "add tests"}),
            ))
            .await
            .unwrap();

        let results = store
            .query_by_type_prefix("orchestrator:", 100)
            .await
            .unwrap();

        assert_eq!(results.len(), 3);
        // All should be orchestrator events
        for event in &results {
            assert!(event.event_type.as_str().starts_with("orchestrator:"));
        }
    }

    #[tokio::test]
    async fn query_by_type_prefix_respects_limit() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        for i in 0..5 {
            store
                .append(&Event::new(
                    EventType::OrchestratorDecision,
                    &format!("task-{}", i),
                    Actor::Orchestrator,
                    serde_json::json!({"approved": true}),
                ))
                .await
                .unwrap();
        }

        let results = store
            .query_by_type_prefix("orchestrator:", 3)
            .await
            .unwrap();

        assert_eq!(results.len(), 3);
    }
}
