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
//!
//! ## Retention & compaction (#470)
//!
//! Event logs are compacted per-task according to a configurable
//! [`RetentionPolicy`]. Compaction keeps only the most recent N events and/or
//! events newer than a max age. Orphaned task directories (those without a
//! corresponding events file) can also be cleaned up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::Event;

/// Current event log format version. Bump when the Event serialization
/// format changes in an incompatible way.
pub const EVENT_FORMAT_VERSION: u32 = 1;

/// Name of the version marker file inside the event store root.
const VERSION_FILE: &str = "format_version";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("event format version mismatch: found v{found}, expected v{expected}")]
    FormatMismatch { found: u32, expected: u32 },
}

/// Configures retention limits for event log compaction.
///
/// Both limits are applied together — an event is kept only if it satisfies
/// *both* constraints. Set a field to `None` to disable that limit.
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// Maximum number of events to keep per task log.
    pub max_events: Option<usize>,
    /// Maximum age of events to keep.
    pub max_age: Option<Duration>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_events: Some(10_000),
            max_age: Some(Duration::from_secs(30 * 24 * 3600)), // 30 days
        }
    }
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
    /// Retention policy for compaction.
    retention: RetentionPolicy,
}

impl EventStore {
    /// Create a new event store at the given root directory with default
    /// retention policy.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self::with_retention(root, RetentionPolicy::default())
    }

    /// Create a new event store with a custom retention policy.
    pub fn with_retention(root: impl AsRef<Path>, retention: RetentionPolicy) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            write_locks: Mutex::new(HashMap::new()),
            retention,
        }
    }

    /// Check the event format version and stamp it for new stores.
    ///
    /// Call this once at startup after creating the store. Returns
    /// [`StoreError::FormatMismatch`] if an existing store has a
    /// different version.
    pub fn check_version(&self) -> Result<(), StoreError> {
        let path = self.root.join(VERSION_FILE);

        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            let stored: u32 = contents.trim().parse().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid event format version file: {contents:?}"),
                )
            })?;

            if stored != EVENT_FORMAT_VERSION {
                return Err(StoreError::FormatMismatch {
                    found: stored,
                    expected: EVENT_FORMAT_VERSION,
                });
            }
        } else {
            // New store — create the version file.
            std::fs::create_dir_all(&self.root)?;
            std::fs::write(&path, format!("{EVENT_FORMAT_VERSION}\n"))?;
        }

        Ok(())
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
                "completed read with corrupted lines skipped"
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

    /// Compact event logs for all tasks according to the retention policy.
    ///
    /// Returns the number of events removed across all tasks.
    pub async fn compact_all(&self) -> Result<usize, StoreError> {
        let task_ids = self.list_tasks().await?;
        let mut total_removed = 0;

        for task_id in &task_ids {
            total_removed += self.compact_task(task_id).await?;
        }

        // Also compact system-wide events (empty task ID).
        total_removed += self.compact_task("").await?;

        if total_removed > 0 {
            tracing::info!(
                removed = total_removed,
                tasks = task_ids.len(),
                "event log compaction complete"
            );
        }

        Ok(total_removed)
    }

    /// Compact a single task's event log according to the retention policy.
    ///
    /// Rewrites the JSONL file in-place (via atomic rename) keeping only
    /// events that satisfy both the `max_events` and `max_age` limits.
    /// Returns the number of events removed.
    pub async fn compact_task(&self, task_id: &str) -> Result<usize, StoreError> {
        let path = self.task_log_path(task_id);
        if !path.exists() {
            return Ok(0);
        }

        let lock = self.task_lock(task_id).await;
        let _guard = lock.lock().await;

        // Read all events.
        let events = self.read_task_unlocked(&path).await?;
        let original_count = events.len();

        // Apply retention policy.
        let kept = self.apply_retention(events);
        let removed = original_count - kept.len();

        if removed == 0 {
            return Ok(0);
        }

        // Write retained events to a temp file, then atomically rename.
        let tmp_path = path.with_extension("jsonl.tmp");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .await?;

            for event in &kept {
                let mut line = serde_json::to_string(event)?;
                line.push('\n');
                file.write_all(line.as_bytes()).await?;
            }
            file.flush().await?;
        }

        fs::rename(&tmp_path, &path).await?;

        tracing::debug!(
            task_id,
            original = original_count,
            kept = kept.len(),
            removed,
            "compacted task event log"
        );

        Ok(removed)
    }

    /// Remove task directories that have no events file or are empty.
    ///
    /// Returns the number of directories removed.
    pub async fn cleanup_orphaned_tasks(&self) -> Result<usize, StoreError> {
        if !self.root.exists() {
            return Ok(0);
        }

        let mut removed = 0;
        let mut entries = fs::read_dir(&self.root).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Skip the version file or non-task entries.
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let events_file = path.join("events.jsonl");
            let should_remove = if events_file.exists() {
                // Remove if the events file is empty (0 bytes).
                let metadata = fs::metadata(&events_file).await?;
                metadata.len() == 0
            } else {
                // No events file at all — check if directory is empty.
                let mut dir = fs::read_dir(&path).await?;
                dir.next_entry().await?.is_none()
            };

            if should_remove {
                if let Err(e) = fs::remove_dir_all(&path).await {
                    tracing::warn!(
                        task_id = name,
                        error = %e,
                        "failed to remove orphaned task directory"
                    );
                } else {
                    tracing::debug!(task_id = name, "removed orphaned task directory");
                    removed += 1;
                }
            }
        }

        if removed > 0 {
            // Clean up write locks for removed tasks.
            let mut locks = self.write_locks.lock().await;
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }

        Ok(removed)
    }

    /// Read events from a path without acquiring the task lock.
    ///
    /// Caller must hold the lock.
    async fn read_task_unlocked(&self, path: &Path) -> Result<Vec<Event>, StoreError> {
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut events = Vec::new();

        while let Some(line) = lines.next_line().await? {
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                events.push(event);
            }
        }

        Ok(events)
    }

    /// Apply retention policy to a list of events, returning the events to keep.
    fn apply_retention(&self, mut events: Vec<Event>) -> Vec<Event> {
        // Apply max_age: drop events older than the cutoff.
        if let Some(max_age) = self.retention.max_age {
            let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::MAX);
            events.retain(|e| e.ts >= cutoff);
        }

        // Apply max_events: keep only the most recent N.
        if let Some(max) = self.retention.max_events {
            if events.len() > max {
                events = events.split_off(events.len() - max);
            }
        }

        events
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

    #[tokio::test]
    async fn compact_task_enforces_max_events() {
        let dir = tempdir().unwrap();
        let policy = RetentionPolicy {
            max_events: Some(3),
            max_age: None,
        };
        let store = EventStore::with_retention(dir.path(), policy);

        // Write 10 events.
        for i in 0..10 {
            store
                .append(&Event::new(
                    EventType::AgentMessage,
                    "task-1",
                    Actor::Agent,
                    serde_json::json!({"index": i}),
                ))
                .await
                .unwrap();
        }

        let removed = store.compact_task("task-1").await.unwrap();
        assert_eq!(removed, 7);

        let events = store.read_task("task-1").await.unwrap();
        assert_eq!(events.len(), 3);

        // Should be the last 3 events.
        let indices: Vec<i64> = events
            .iter()
            .map(|e| e.data["index"].as_i64().unwrap())
            .collect();
        assert_eq!(indices, vec![7, 8, 9]);
    }

    #[tokio::test]
    async fn compact_task_enforces_max_age() {
        let dir = tempdir().unwrap();
        let policy = RetentionPolicy {
            max_events: None,
            max_age: Some(Duration::from_secs(60)), // 1 minute
        };
        let store = EventStore::with_retention(dir.path(), policy);

        // Create an old event by manipulating the timestamp.
        let mut old_event = Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({"old": true}),
        );
        old_event.ts = Utc::now() - chrono::Duration::seconds(3600); // 1 hour ago

        // Write old event directly to disk.
        let path = dir.path().join("task-1");
        std::fs::create_dir_all(&path).unwrap();
        let mut line = serde_json::to_string(&old_event).unwrap();
        line.push('\n');
        std::fs::write(path.join("events.jsonl"), &line).unwrap();

        // Write a fresh event through the store.
        store
            .append(&Event::new(
                EventType::AgentMessage,
                "task-1",
                Actor::Agent,
                serde_json::json!({"new": true}),
            ))
            .await
            .unwrap();

        let removed = store.compact_task("task-1").await.unwrap();
        assert_eq!(removed, 1);

        let events = store.read_task("task-1").await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].data["new"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn compact_all_processes_all_tasks() {
        let dir = tempdir().unwrap();
        let policy = RetentionPolicy {
            max_events: Some(2),
            max_age: None,
        };
        let store = EventStore::with_retention(dir.path(), policy);

        for task in ["task-a", "task-b"] {
            for i in 0..5 {
                store
                    .append(&Event::new(
                        EventType::AgentMessage,
                        task,
                        Actor::Agent,
                        serde_json::json!({"i": i}),
                    ))
                    .await
                    .unwrap();
            }
        }

        let removed = store.compact_all().await.unwrap();
        assert_eq!(removed, 6); // 3 removed per task

        for task in ["task-a", "task-b"] {
            let events = store.read_task(task).await.unwrap();
            assert_eq!(events.len(), 2);
        }
    }

    #[tokio::test]
    async fn compact_noop_when_within_limits() {
        let dir = tempdir().unwrap();
        let policy = RetentionPolicy {
            max_events: Some(100),
            max_age: None,
        };
        let store = EventStore::with_retention(dir.path(), policy);

        store
            .append(&Event::new(
                EventType::TaskCreated,
                "task-1",
                Actor::System,
                serde_json::json!({}),
            ))
            .await
            .unwrap();

        let removed = store.compact_task("task-1").await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn cleanup_orphaned_removes_empty_dirs() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        // Create an orphaned directory with no events file.
        std::fs::create_dir_all(dir.path().join("orphan-task")).unwrap();

        // Create a valid task.
        store
            .append(&Event::new(
                EventType::TaskCreated,
                "valid-task",
                Actor::System,
                serde_json::json!({}),
            ))
            .await
            .unwrap();

        let removed = store.cleanup_orphaned_tasks().await.unwrap();
        assert_eq!(removed, 1);

        // Valid task still exists.
        let tasks = store.list_tasks().await.unwrap();
        assert_eq!(tasks, vec!["valid-task"]);

        // Orphan is gone.
        assert!(!dir.path().join("orphan-task").exists());
    }

    #[tokio::test]
    async fn cleanup_orphaned_removes_empty_events_file() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());

        // Create a task directory with an empty events file.
        let task_dir = dir.path().join("empty-task");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("events.jsonl"), "").unwrap();

        let removed = store.cleanup_orphaned_tasks().await.unwrap();
        assert_eq!(removed, 1);
        assert!(!task_dir.exists());
    }
}
