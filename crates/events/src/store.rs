//! Append-only event storage.
//!
//! Events are stored per-task as JSONL files: `<task-id>/events.jsonl`
//!
//! ## Reliability
//!
//! - **Corrupted line resilience (#469):** `read_task()` skips malformed JSON
//!   lines with a warning instead of failing the entire read.
//! - **Concurrent write protection (#468):** Writes to the same task file are
//!   serialized via a per-task `tokio::sync::Mutex` to prevent byte interleaving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

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
///
/// Concurrent writes to the same task file are serialized via a per-task
/// mutex to prevent byte interleaving.
pub struct EventStore {
    root: PathBuf,
    /// Per-task write locks to serialize concurrent appends to the same file.
    write_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl EventStore {
    /// Create a new event store at the given root directory.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            write_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Get or create the write lock for a given task ID.
    async fn task_lock(&self, task_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.write_locks.lock().await;
        locks
            .entry(task_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Path to a task's event log file.
    fn task_log_path(&self, task_id: &str) -> PathBuf {
        self.root.join(task_id).join("events.jsonl")
    }

    /// Append an event to the store.
    ///
    /// Creates the task directory if it doesn't exist. Concurrent writes to
    /// the same task file are serialized via a per-task mutex to prevent byte
    /// interleaving.
    pub async fn append(&self, event: &Event) -> Result<(), StoreError> {
        let path = self.task_log_path(&event.task);
        let lock = self.task_lock(&event.task).await;

        // Serialize the JSON before acquiring the lock to minimize hold time.
        let mut line = serde_json::to_string(event)?;
        line.push('\n');

        let _guard = lock.lock().await;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;

        file.write_all(line.as_bytes()).await?;
        file.flush().await?;

        Ok(())
    }

    /// Read all events for a task.
    ///
    /// Malformed JSON lines are skipped with a warning instead of failing
    /// the entire read. Empty lines are silently skipped.
    pub async fn read_task(&self, task_id: &str) -> Result<Vec<Event>, StoreError> {
        let path = self.task_log_path(task_id);

        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut events = Vec::new();
        let mut line_number: usize = 0;
        let mut skipped: usize = 0;

        while let Some(line) = lines.next_line().await? {
            line_number += 1;

            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<Event>(&line) {
                Ok(event) => events.push(event),
                Err(err) => {
                    skipped += 1;
                    let truncated: String = line.chars().take(200).collect();
                    tracing::warn!(
                        task_id,
                        line_number,
                        error = %err,
                        raw_line = truncated,
                        "skipping corrupted JSONL line"
                    );
                }
            }
        }

        if skipped > 0 {
            tracing::warn!(
                task_id,
                total_events = events.len(),
                skipped,
                "read {} events for task {} ({} lines skipped due to corruption)",
                events.len(),
                task_id,
                skipped,
            );
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
    use std::io::Write;
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

    #[tokio::test]
    async fn read_task_skips_corrupted_lines() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        // Write two valid events through the store.
        let event1 = Event::new(
            EventType::TaskCreated,
            "task-corrupt",
            Actor::System,
            serde_json::json!({"title": "First"}),
        );
        let event2 = Event::new(
            EventType::TaskUpdated,
            "task-corrupt",
            Actor::System,
            serde_json::json!({"title": "Second"}),
        );
        store.append(&event1).await.unwrap();
        store.append(&event2).await.unwrap();

        // Now inject a corrupted line directly into the JSONL file between
        // existing events and an additional valid event.
        let log_path = dir.path().join("task-corrupt").join("events.jsonl");
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .unwrap();
            // Corrupted line (not valid JSON)
            writeln!(file, "{{this is not valid json}}").unwrap();
            // Another valid event
            let event3 = Event::new(
                EventType::AgentMessage,
                "task-corrupt",
                Actor::Agent,
                serde_json::json!({"message": "Third"}),
            );
            let json = serde_json::to_string(&event3).unwrap();
            writeln!(file, "{}", json).unwrap();
        }

        // read_task should return the 3 valid events, skipping the corrupted line.
        let events = store.read_task("task-corrupt").await.unwrap();
        assert_eq!(
            events.len(),
            3,
            "expected 3 valid events, got {}",
            events.len()
        );
        assert_eq!(events[0].id, event1.id);
        assert_eq!(events[1].id, event2.id);
        // The third event was written directly, so we just verify its type.
        assert_eq!(events[2].event_type, EventType::AgentMessage);
    }

    #[tokio::test]
    async fn read_task_skips_corrupted_but_not_empty_lines() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        let event = Event::new(
            EventType::TaskCreated,
            "task-empty",
            Actor::System,
            serde_json::json!({}),
        );
        store.append(&event).await.unwrap();

        // Inject empty lines and a corrupted line.
        let log_path = dir.path().join("task-empty").join("events.jsonl");
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .unwrap();
            writeln!(file).unwrap(); // empty line
            writeln!(file).unwrap(); // empty line
            writeln!(file, "CORRUPTED").unwrap(); // bad line
            writeln!(file).unwrap(); // empty line
        }

        let events = store.read_task("task-empty").await.unwrap();
        assert_eq!(events.len(), 1, "expected 1 valid event");
        assert_eq!(events[0].id, event.id);
    }

    #[tokio::test]
    async fn concurrent_appends_do_not_corrupt() {
        let dir = tempdir().unwrap();
        let store = Arc::new(EventStore::new(dir.path()));
        let task_id = "task-concurrent";

        // Spawn many concurrent appends to the same task.
        let mut handles = Vec::new();
        for i in 0..20 {
            let store = Arc::clone(&store);
            let task_id = task_id.to_string();
            handles.push(tokio::spawn(async move {
                let event = Event::new(
                    EventType::AgentMessage,
                    &task_id,
                    Actor::Agent,
                    serde_json::json!({"index": i}),
                );
                store.append(&event).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // All 20 events should be readable without corruption.
        let events = store.read_task(task_id).await.unwrap();
        assert_eq!(events.len(), 20, "expected 20 events, got {}", events.len());
    }
}
