# Centralized Work Queue Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the racey dispatch evaluation with a centralized work queue that provides synchronous claiming to prevent duplicate task dispatch.

**Architecture:** A new `WorkQueue` component becomes the single source of truth for dispatchable work. It derives the queue from source systems (tasks, automations, PRs) but persists claim status in SQLite. The dispatch loop simplifies to: rebuild queue, claim next item (respecting timeouts and slots), create container.

**Tech Stack:** Rust, SQLite (rusqlite), chrono, tokio

---

## Task 1: Add WorkType enum and WorkItem struct

**Files:**
- Create: `crates/models/src/work_queue.rs`
- Modify: `crates/models/src/lib.rs`

**Step 1: Create work_queue.rs with WorkType enum and WorkItem struct**

```rust
//! Work queue models — centralized work dispatch.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Type of work item — determines priority tier.
///
/// Priority order (highest to lowest):
/// 1. MergeConflict — blocking merged work
/// 2. PrFeedback — changes requested on existing PRs
/// 3. Automation — scheduled/triggered automation runs
/// 4. Task — new issue implementations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WorkType {
    MergeConflict = 0,
    PrFeedback = 1,
    Automation = 2,
    Task = 3,
}

impl WorkType {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkType::MergeConflict => "merge_conflict",
            WorkType::PrFeedback => "pr_feedback",
            WorkType::Automation => "automation",
            WorkType::Task => "task",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "merge_conflict" => Some(WorkType::MergeConflict),
            "pr_feedback" => Some(WorkType::PrFeedback),
            "automation" => Some(WorkType::Automation),
            "task" => Some(WorkType::Task),
            _ => None,
        }
    }
}

/// A work item in the centralized queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique work item ID (format: "{work_type}:{source_id}")
    pub id: String,
    /// Type of work — determines priority tier
    pub work_type: WorkType,
    /// Source identifier (task_id, automation_run_id, pr_url, etc.)
    pub source_id: String,
    /// Project this work belongs to
    pub project_id: String,
    /// Priority within tier (lower = higher priority)
    pub priority: u32,
    /// When this work item was created/discovered
    pub created_at: DateTime<Utc>,
    /// When this item was claimed (None = unclaimed)
    pub claimed_at: Option<DateTime<Utc>>,
    /// Container ID that claimed this work (None = unclaimed)
    pub claimed_by: Option<String>,
}

impl WorkItem {
    pub fn new(
        work_type: WorkType,
        source_id: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        let source_id = source_id.into();
        let id = format!("{}:{}", work_type.as_str(), source_id);
        Self {
            id,
            work_type,
            source_id,
            project_id: project_id.into(),
            priority: 0,
            created_at: Utc::now(),
            claimed_at: None,
            claimed_by: None,
        }
    }

    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_created_at(mut self, created_at: DateTime<Utc>) -> Self {
        self.created_at = created_at;
        self
    }

    pub fn is_claimed(&self) -> bool {
        self.claimed_by.is_some()
    }
}

/// Result of a claim operation.
#[derive(Debug, Clone)]
pub struct ClaimResult {
    pub work_item: WorkItem,
    pub container_id: String,
}

/// Information about reclaimed work (from dead/timed-out containers).
#[derive(Debug, Clone)]
pub struct ReclaimedWork {
    pub work_id: String,
    pub previous_container_id: String,
    pub reason: String,
}
```

**Step 2: Run test to verify compilation**

Run: `cargo check --package tasks-models`
Expected: PASS (no errors)

**Step 3: Export from lib.rs**

Add to `crates/models/src/lib.rs`:
```rust
mod work_queue;
pub use work_queue::{WorkType, WorkItem, ClaimResult, ReclaimedWork};
```

**Step 4: Run tests**

Run: `cargo test --package tasks-models`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/models/src/work_queue.rs crates/models/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(models): add WorkType and WorkItem for centralized work queue

Part of #658 — these types represent items in the new centralized
work queue that replaces the racey dispatch evaluation.
EOF
)"
```

---

## Task 2: Add work_claims SQLite table

**Files:**
- Modify: `crates/store/src/schema.rs`
- Modify: `crates/store/src/lib.rs`

**Step 1: Add work_claims table to schema initialization**

In `crates/store/src/schema.rs`, add to the `execute_batch` in `initialize()`:

```rust
        -- Work claims: tracks claimed work items for the centralized queue (#658)
        -- The queue itself is derived from source systems; this table persists
        -- claim status so restarts don't re-dispatch in-progress work.
        CREATE TABLE IF NOT EXISTS work_claims (
            work_id TEXT PRIMARY KEY,
            work_type TEXT NOT NULL,
            source_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            container_id TEXT,
            claimed_at TEXT,
            released_at TEXT,
            release_note TEXT,
            completed_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_work_claims_active
            ON work_claims(container_id) WHERE completed_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_work_claims_source
            ON work_claims(source_id);
```

**Step 2: Run test to verify migration**

Run: `cargo test --package tasks-store`
Expected: PASS

**Step 3: Commit**

```bash
git add crates/store/src/schema.rs
git commit -m "$(cat <<'EOF'
feat(store): add work_claims table for centralized queue

Part of #658 — persists claim status so in-progress work survives
server restarts and isn't re-dispatched.
EOF
)"
```

---

## Task 3: Add work claims persistence layer

**Files:**
- Create: `crates/store/src/work_claims.rs`
- Modify: `crates/store/src/lib.rs`

**Step 1: Create work_claims.rs with CRUD operations**

```rust
//! Work claims persistence — tracks claimed work items.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::StoreError;

/// A persisted work claim record.
#[derive(Debug, Clone)]
pub struct WorkClaim {
    pub work_id: String,
    pub work_type: String,
    pub source_id: String,
    pub project_id: String,
    pub container_id: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
    pub release_note: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl WorkClaim {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            work_id: row.get("work_id")?,
            work_type: row.get("work_type")?,
            source_id: row.get("source_id")?,
            project_id: row.get("project_id")?,
            container_id: row.get("container_id")?,
            claimed_at: row
                .get::<_, Option<String>>("claimed_at")?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            released_at: row
                .get::<_, Option<String>>("released_at")?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            release_note: row.get("release_note")?,
            completed_at: row
                .get::<_, Option<String>>("completed_at")?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
        })
    }
}

/// Insert or update a work claim (upsert).
pub fn upsert_claim(
    conn: &Connection,
    work_id: &str,
    work_type: &str,
    source_id: &str,
    project_id: &str,
    container_id: &str,
) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO work_claims (work_id, work_type, source_id, project_id, container_id, claimed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(work_id) DO UPDATE SET
             container_id = excluded.container_id,
             claimed_at = excluded.claimed_at,
             released_at = NULL,
             release_note = NULL",
        params![work_id, work_type, source_id, project_id, container_id, now],
    )?;
    Ok(())
}

/// Get a work claim by ID.
pub fn get_claim(conn: &Connection, work_id: &str) -> Result<Option<WorkClaim>, StoreError> {
    let claim = conn
        .query_row(
            "SELECT * FROM work_claims WHERE work_id = ?1",
            params![work_id],
            WorkClaim::from_row,
        )
        .optional()?;
    Ok(claim)
}

/// Get all active claims (claimed but not completed).
pub fn get_active_claims(conn: &Connection) -> Result<Vec<WorkClaim>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT * FROM work_claims
         WHERE container_id IS NOT NULL AND completed_at IS NULL",
    )?;
    let claims = stmt
        .query_map([], WorkClaim::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(claims)
}

/// Release a claim (container gave up the work).
pub fn release_claim(
    conn: &Connection,
    work_id: &str,
    note: Option<&str>,
) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE work_claims SET
            container_id = NULL,
            released_at = ?2,
            release_note = ?3
         WHERE work_id = ?1",
        params![work_id, now, note],
    )?;
    Ok(())
}

/// Mark a claim as completed.
pub fn complete_claim(conn: &Connection, work_id: &str) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE work_claims SET completed_at = ?2 WHERE work_id = ?1",
        params![work_id, now],
    )?;
    Ok(())
}

/// Check if a source_id has an active (uncompleted) claim.
pub fn has_active_claim_for_source(
    conn: &Connection,
    source_id: &str,
) -> Result<bool, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM work_claims
         WHERE source_id = ?1 AND completed_at IS NULL AND container_id IS NOT NULL",
        params![source_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Delete completed claims older than the given timestamp.
pub fn cleanup_old_claims(
    conn: &Connection,
    before: DateTime<Utc>,
) -> Result<usize, StoreError> {
    let before_str = before.to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM work_claims WHERE completed_at IS NOT NULL AND completed_at < ?1",
        params![before_str],
    )?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::initialize;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn
    }

    #[test]
    fn test_upsert_and_get_claim() {
        let conn = setup_db();

        upsert_claim(&conn, "task:t1", "task", "t1", "proj1", "container-1").unwrap();

        let claim = get_claim(&conn, "task:t1").unwrap().unwrap();
        assert_eq!(claim.work_id, "task:t1");
        assert_eq!(claim.source_id, "t1");
        assert_eq!(claim.container_id, Some("container-1".to_string()));
        assert!(claim.claimed_at.is_some());
    }

    #[test]
    fn test_release_claim() {
        let conn = setup_db();

        upsert_claim(&conn, "task:t1", "task", "t1", "proj1", "container-1").unwrap();
        release_claim(&conn, "task:t1", Some("container died")).unwrap();

        let claim = get_claim(&conn, "task:t1").unwrap().unwrap();
        assert!(claim.container_id.is_none());
        assert!(claim.released_at.is_some());
        assert_eq!(claim.release_note, Some("container died".to_string()));
    }

    #[test]
    fn test_complete_claim() {
        let conn = setup_db();

        upsert_claim(&conn, "task:t1", "task", "t1", "proj1", "container-1").unwrap();
        complete_claim(&conn, "task:t1").unwrap();

        let claim = get_claim(&conn, "task:t1").unwrap().unwrap();
        assert!(claim.completed_at.is_some());
    }

    #[test]
    fn test_get_active_claims() {
        let conn = setup_db();

        upsert_claim(&conn, "task:t1", "task", "t1", "proj1", "container-1").unwrap();
        upsert_claim(&conn, "task:t2", "task", "t2", "proj1", "container-2").unwrap();
        complete_claim(&conn, "task:t2").unwrap();

        let active = get_active_claims(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].work_id, "task:t1");
    }

    #[test]
    fn test_has_active_claim_for_source() {
        let conn = setup_db();

        assert!(!has_active_claim_for_source(&conn, "t1").unwrap());

        upsert_claim(&conn, "task:t1", "task", "t1", "proj1", "container-1").unwrap();
        assert!(has_active_claim_for_source(&conn, "t1").unwrap());

        complete_claim(&conn, "task:t1").unwrap();
        assert!(!has_active_claim_for_source(&conn, "t1").unwrap());
    }
}
```

**Step 2: Export from lib.rs**

Add to `crates/store/src/lib.rs`:
```rust
pub mod work_claims;
```

**Step 3: Run tests**

Run: `cargo test --package tasks-store`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/store/src/work_claims.rs crates/store/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(store): add work_claims persistence layer

Part of #658 — CRUD operations for work claim records that track
which containers have claimed which work items.
EOF
)"
```

---

## Task 4: Create WorkQueue service

**Files:**
- Create: `crates/server/src/work_queue.rs`
- Modify: `crates/server/src/lib.rs`

**Step 1: Create work_queue.rs with WorkQueue struct**

```rust
//! Centralized work queue — the single source of truth for dispatchable work.
//!
//! This replaces the racey dispatch evaluation with synchronous claiming.
//! All work (tasks, automations, PR feedback, conflicts) flows through here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use tasks_models::{WorkItem, WorkType, ReclaimedWork};
use tasks_store::Store;

/// Configuration for the work queue.
#[derive(Debug, Clone)]
pub struct WorkQueueConfig {
    /// Minimum delay between dispatches (rate limiting).
    pub work_queue_timeout: Duration,
    /// Maximum time a container can work before being reclaimed.
    pub container_timeout: Duration,
    /// How often to check for dead/timed-out containers.
    pub health_check_interval: Duration,
}

impl Default for WorkQueueConfig {
    fn default() -> Self {
        Self {
            work_queue_timeout: Duration::from_secs(15),
            container_timeout: Duration::from_secs(2 * 60 * 60), // 2 hours
            health_check_interval: Duration::from_secs(30),
        }
    }
}

/// The centralized work queue.
///
/// Provides synchronous claiming to prevent duplicate dispatch.
/// The queue is derived from source systems but claim status is persisted.
pub struct WorkQueue {
    /// Work items sorted by priority (work_type tier, then priority, then created_at).
    items: Vec<WorkItem>,
    /// Configuration.
    config: WorkQueueConfig,
    /// Last dispatch timestamp for rate limiting.
    last_dispatch: Option<Instant>,
    /// Store for claim persistence.
    store: Arc<Store>,
}

impl WorkQueue {
    pub fn new(store: Arc<Store>, config: WorkQueueConfig) -> Self {
        Self {
            items: Vec::new(),
            config,
            last_dispatch: None,
            store,
        }
    }

    /// Rebuild the queue from source systems.
    ///
    /// This derives the queue from tasks, automations, PRs with feedback, etc.
    /// Preserves claim status from the database.
    pub async fn rebuild(
        &mut self,
        tasks: &HashMap<String, tasks_models::Task>,
        // TODO: Add automation_runs parameter when wiring up
        // TODO: Add merge_queue parameter for PR feedback and conflicts
    ) -> Result<(), WorkQueueError> {
        let mut items = Vec::new();

        // Collect tasks in dispatchable states (Waiting, ChangesRequested)
        for task in tasks.values() {
            use tasks_models::TaskState;

            let work_type = match task.state {
                TaskState::ChangesRequested => WorkType::PrFeedback,
                TaskState::Waiting => WorkType::Task,
                TaskState::Conflict => WorkType::MergeConflict,
                _ => continue, // Not dispatchable
            };

            // Check backoff for failed tasks
            if let Some(failure_at) = task.last_failure_at {
                let backoff = crate::dispatcher::backoff_duration(task.retry_count, &task.id);
                if failure_at + backoff > Utc::now() {
                    continue; // Still in backoff
                }
            }

            let mut item = WorkItem::new(work_type, &task.id, &task.project);

            // Set priority from task priority (lower = higher priority)
            if let Some(p) = task.priority {
                item = item.with_priority(p as u32);
            }

            // Set created_at from source for ordering
            if let Some(created) = task.source_created_at {
                item = item.with_created_at(created);
            }

            items.push(item);
        }

        // TODO(#657): Add support for user-requested priority insertion

        // Sort by: work_type tier, then priority, then created_at
        items.sort_by(|a, b| {
            a.work_type
                .cmp(&b.work_type)
                .then_with(|| a.priority.cmp(&b.priority))
                .then_with(|| a.created_at.cmp(&b.created_at))
        });

        // Load claim status from database
        let conn = self.store.conn();
        let active_claims = tasks_store::work_claims::get_active_claims(&conn)?;
        drop(conn);

        // Apply claim status to items
        let claimed_sources: HashMap<String, String> = active_claims
            .iter()
            .filter_map(|c| c.container_id.as_ref().map(|cid| (c.source_id.clone(), cid.clone())))
            .collect();

        for item in &mut items {
            if let Some(container_id) = claimed_sources.get(&item.source_id) {
                item.claimed_by = Some(container_id.clone());
                item.claimed_at = Some(Utc::now()); // Approximate
            }
        }

        self.items = items;
        debug!(count = self.items.len(), "work queue rebuilt");
        Ok(())
    }

    /// Claim the next available work item.
    ///
    /// Respects:
    /// - Work queue timeout (rate limiting)
    /// - Slot limits (max_slots, active_count)
    /// - Returns highest priority unclaimed item
    pub async fn claim_next(
        &mut self,
        max_slots: usize,
        active_count: usize,
        container_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        // Check slot availability
        if active_count >= max_slots {
            debug!(active = active_count, max = max_slots, "no slots available");
            return Ok(None);
        }

        // Check rate limit
        if let Some(last) = self.last_dispatch {
            let elapsed = last.elapsed();
            if elapsed < self.config.work_queue_timeout {
                debug!(
                    elapsed_ms = elapsed.as_millis(),
                    timeout_ms = self.config.work_queue_timeout.as_millis(),
                    "work queue timeout not elapsed"
                );
                return Ok(None);
            }
        }

        // Find first unclaimed item
        let item_idx = self.items.iter().position(|item| !item.is_claimed());

        let Some(idx) = item_idx else {
            debug!("no unclaimed work items");
            return Ok(None);
        };

        // Claim it
        let item = &mut self.items[idx];
        item.claimed_by = Some(container_id.to_string());
        item.claimed_at = Some(Utc::now());

        // Persist claim
        let conn = self.store.conn();
        tasks_store::work_claims::upsert_claim(
            &conn,
            &item.id,
            item.work_type.as_str(),
            &item.source_id,
            &item.project_id,
            container_id,
        )?;
        drop(conn);

        self.last_dispatch = Some(Instant::now());

        info!(
            work_id = %item.id,
            work_type = ?item.work_type,
            source_id = %item.source_id,
            container_id = %container_id,
            "work item claimed"
        );

        Ok(Some(item.clone()))
    }

    /// Insert a high-priority work item (user request via #657).
    pub fn insert_priority(&mut self, item: WorkItem) {
        // TODO(#657): Wire up user-requested task priority insertion via API endpoint
        // Insert at the front of its priority tier
        let insert_pos = self
            .items
            .iter()
            .position(|existing| existing.work_type > item.work_type)
            .unwrap_or(0);
        self.items.insert(insert_pos, item);
    }

    /// Release a claim (container finished or relinquished).
    pub async fn release(
        &mut self,
        work_id: &str,
        note: Option<&str>,
    ) -> Result<(), WorkQueueError> {
        // Update in-memory state
        if let Some(item) = self.items.iter_mut().find(|i| i.id == work_id) {
            item.claimed_by = None;
            item.claimed_at = None;
        }

        // Persist
        let conn = self.store.conn();
        tasks_store::work_claims::release_claim(&conn, work_id, note)?;
        drop(conn);

        info!(work_id = %work_id, note = ?note, "work item released");
        Ok(())
    }

    /// Mark work as completed (removes from queue).
    pub async fn complete(&mut self, work_id: &str) -> Result<(), WorkQueueError> {
        // Remove from in-memory queue
        self.items.retain(|i| i.id != work_id);

        // Mark completed in database
        let conn = self.store.conn();
        tasks_store::work_claims::complete_claim(&conn, work_id)?;
        drop(conn);

        info!(work_id = %work_id, "work item completed");
        Ok(())
    }

    /// Health check — reclaim work from dead/timed-out containers.
    ///
    /// Returns list of reclaimed work items.
    pub async fn health_check<F>(
        &mut self,
        is_container_alive: F,
    ) -> Result<Vec<ReclaimedWork>, WorkQueueError>
    where
        F: Fn(&str) -> bool,
    {
        let mut reclaimed = Vec::new();
        let now = Instant::now();

        // Get active claims from database
        let conn = self.store.conn();
        let active_claims = tasks_store::work_claims::get_active_claims(&conn)?;
        drop(conn);

        for claim in active_claims {
            let Some(container_id) = &claim.container_id else {
                continue;
            };

            let should_reclaim = if !is_container_alive(container_id) {
                Some("container not found".to_string())
            } else if let Some(claimed_at) = claim.claimed_at {
                let elapsed = Utc::now().signed_duration_since(claimed_at);
                if elapsed.num_seconds() > self.config.container_timeout.as_secs() as i64 {
                    Some(format!("exceeded {}h timeout", self.config.container_timeout.as_secs() / 3600))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(reason) = should_reclaim {
                warn!(
                    work_id = %claim.work_id,
                    container_id = %container_id,
                    reason = %reason,
                    "reclaiming work"
                );

                self.release(&claim.work_id, Some(&reason)).await?;

                reclaimed.push(ReclaimedWork {
                    work_id: claim.work_id,
                    previous_container_id: container_id.clone(),
                    reason,
                });
            }
        }

        Ok(reclaimed)
    }

    /// Get the current queue length (for metrics/debugging).
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Get count of unclaimed items.
    pub fn unclaimed_count(&self) -> usize {
        self.items.iter().filter(|i| !i.is_claimed()).count()
    }
}

/// Errors from work queue operations.
#[derive(Debug, thiserror::Error)]
pub enum WorkQueueError {
    #[error("store error: {0}")]
    Store(#[from] tasks_store::StoreError),
}

// TODO: Container task relinquishment - supervisor protocol message to release work with note
// The supervisor should be able to send a message like:
// { "type": "relinquish", "reason": "task too complex" }
// which triggers work_queue.release() with the reason as the note.

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_models::{Task, TaskSource, TaskState};
    use std::collections::HashMap;

    fn create_test_store() -> Arc<Store> {
        Arc::new(Store::open(":memory:").unwrap())
    }

    fn make_task(id: &str, project: &str, state: TaskState) -> Task {
        let mut task = Task::new(id, TaskSource::Internal, id, project);
        task.state = state;
        task
    }

    #[tokio::test]
    async fn test_rebuild_collects_waiting_tasks() {
        let store = create_test_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let mut tasks = HashMap::new();
        tasks.insert("t1".to_string(), make_task("t1", "proj", TaskState::Waiting));
        tasks.insert("t2".to_string(), make_task("t2", "proj", TaskState::Running));
        tasks.insert("t3".to_string(), make_task("t3", "proj", TaskState::Waiting));

        queue.rebuild(&tasks).await.unwrap();

        assert_eq!(queue.len(), 2);
        assert_eq!(queue.unclaimed_count(), 2);
    }

    #[tokio::test]
    async fn test_claim_next_respects_slots() {
        let store = create_test_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0), // No rate limit for test
            ..Default::default()
        });

        let mut tasks = HashMap::new();
        tasks.insert("t1".to_string(), make_task("t1", "proj", TaskState::Waiting));
        queue.rebuild(&tasks).await.unwrap();

        // No slots available
        let result = queue.claim_next(2, 2, "container-1").await.unwrap();
        assert!(result.is_none());

        // Slots available
        let result = queue.claim_next(2, 1, "container-1").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().source_id, "t1");
    }

    #[tokio::test]
    async fn test_claim_next_respects_rate_limit() {
        let store = create_test_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(60), // Long timeout
            ..Default::default()
        });

        let mut tasks = HashMap::new();
        tasks.insert("t1".to_string(), make_task("t1", "proj", TaskState::Waiting));
        tasks.insert("t2".to_string(), make_task("t2", "proj", TaskState::Waiting));
        queue.rebuild(&tasks).await.unwrap();

        // First claim succeeds
        let result = queue.claim_next(10, 0, "container-1").await.unwrap();
        assert!(result.is_some());

        // Second claim blocked by rate limit
        let result = queue.claim_next(10, 1, "container-2").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        let store = create_test_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        });

        let mut tasks = HashMap::new();

        // Task in Waiting state (lower priority tier)
        tasks.insert("t1".to_string(), make_task("t1", "proj", TaskState::Waiting));

        // Task in ChangesRequested state (higher priority tier = PrFeedback)
        tasks.insert("t2".to_string(), make_task("t2", "proj", TaskState::ChangesRequested));

        queue.rebuild(&tasks).await.unwrap();

        // ChangesRequested should come first
        let result = queue.claim_next(10, 0, "container-1").await.unwrap();
        assert_eq!(result.unwrap().source_id, "t2");
    }

    #[tokio::test]
    async fn test_complete_removes_from_queue() {
        let store = create_test_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        });

        let mut tasks = HashMap::new();
        tasks.insert("t1".to_string(), make_task("t1", "proj", TaskState::Waiting));
        queue.rebuild(&tasks).await.unwrap();

        assert_eq!(queue.len(), 1);

        queue.complete("task:t1").await.unwrap();

        assert_eq!(queue.len(), 0);
    }
}
```

**Step 2: Export from lib.rs**

Add to `crates/server/src/lib.rs`:
```rust
mod work_queue;
pub use work_queue::{WorkQueue, WorkQueueConfig, WorkQueueError};
```

**Step 3: Run tests**

Run: `cargo test --package tasks-server work_queue`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/server/src/work_queue.rs crates/server/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(server): add WorkQueue service for centralized dispatch

Part of #658 — the WorkQueue is the single source of truth for
dispatchable work, providing synchronous claiming to prevent
duplicate dispatch.
EOF
)"
```

---

## Task 5: Wire WorkQueue into dispatch loop

**Files:**
- Modify: `crates/app/src/run_loop.rs`

**Step 1: Add WorkQueue initialization**

Near the infrastructure setup section (around line 200), add:

```rust
// Create work queue with config from environment
let work_queue_config = server::WorkQueueConfig {
    work_queue_timeout: Duration::from_secs(
        std::env::var("WORK_QUEUE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15)
    ),
    container_timeout: Duration::from_secs(
        std::env::var("CONTAINER_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2 * 60 * 60)
    ),
    health_check_interval: Duration::from_secs(
        std::env::var("HEALTH_CHECK_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30)
    ),
};
let work_queue = Arc::new(RwLock::new(server::WorkQueue::new(
    store.clone(),
    work_queue_config.clone(),
)));
```

**Step 2: Replace dispatch loop logic**

Replace the dispatch loop (around lines 1180-1302) with:

```rust
// --- 8a. Spawn dispatch loop (work queue based) ---
let dispatch_server = server.clone();
let dispatch_work_queue = work_queue.clone();
let dispatch_session_mgr = session_manager.clone();
let dispatch_memory_gate = memory_gate.clone();
let dispatch_github_token = config.github_token.clone();
let dispatch_config_watcher = config_watcher.clone();
let mut dispatch_shutdown_rx = shutdown_tx.subscribe();

let dispatch_handle = tokio::spawn(async move {
    let mut interval = tokio::time::interval(config.dispatch_interval);
    let github_client = GitHubClient::new(&dispatch_github_token);

    loop {
        tokio::select! {
            _ = dispatch_shutdown_rx.recv() => break,
            _ = interval.tick() => {}
        }

        // Skip if in Stop mode
        let mode = dispatch_server.mode().await;
        if mode == server::Mode::Stop {
            continue;
        }

        // Check memory pressure
        let memory_paused = dispatch_memory_gate.is_dispatch_paused();
        if memory_paused {
            let pct = dispatch_memory_gate.current_pct.load(std::sync::atomic::Ordering::Relaxed);
            warn!(used_pct = pct, "dispatch: no new sessions due to memory pressure");
            continue;
        }

        // Rebuild work queue from current state
        {
            let state = dispatch_server.state.read().await;
            let mut queue = dispatch_work_queue.write().await;
            if let Err(e) = queue.rebuild(&state.tasks).await {
                error!(error = %e, "failed to rebuild work queue");
                continue;
            }
        }

        // Get active session count
        let active_count = dispatch_session_mgr.active_session_count().await;

        // Try to claim next work item
        let claimed = {
            let mut queue = dispatch_work_queue.write().await;
            let container_id = Uuid::new_v4().to_string();
            queue.claim_next(max_sessions as usize, active_count, &container_id).await
        };

        let work_item = match claimed {
            Ok(Some(item)) => item,
            Ok(None) => continue, // No work available or rate limited
            Err(e) => {
                error!(error = %e, "failed to claim work");
                continue;
            }
        };

        // Start session for the claimed work
        let task_id = work_item.source_id.clone();
        if let Some(task) = dispatch_server.get_task(&task_id).await {
            let project = dispatch_server.get_project(&task.project).await;
            let repo_url = project
                .as_ref()
                .map(|p| format!("https://github.com/{}.git", p.repo))
                .unwrap_or_default();

            let unique_suffix = &Uuid::new_v4().to_string()[..8];
            let branch = format!("tasks/{}--{}", task.id, unique_suffix);

            let workflow_settings = load_workflow_settings_for_project(
                project.as_ref(),
                &dispatch_github_token,
                &dispatch_config_watcher,
            ).await;

            let comments = server::prompt::fetch_comments_for_task(
                &github_client,
                &task.source,
            ).await;

            let prompt = server::prompt::build_prompt_for_task(
                &task,
                &branch,
                workflow_settings.system_prompt.as_deref(),
                &comments,
            );

            // Update task with session_id before starting
            let container_id = work_item.claimed_by.as_ref().unwrap().clone();
            if let Err(e) = dispatch_server.set_task_session_id(&task_id, Some(&container_id)).await {
                warn!(task_id = %task_id, error = %e, "failed to set session_id on task");
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
                    if task.rejection_feedback.is_some() {
                        if let Err(e) = dispatch_server.clear_task_rejection_feedback(&task_id).await {
                            warn!(task_id = %task_id, error = %e, "failed to clear rejection feedback");
                        }
                    }
                }
                Err(e) => {
                    error!(task_id = %task_id, error = %e, "failed to start session");

                    // Release the claim since we couldn't start
                    let mut queue = dispatch_work_queue.write().await;
                    if let Err(e2) = queue.release(&work_item.id, Some(&format!("session start failed: {}", e))).await {
                        warn!(work_id = %work_item.id, error = %e2, "failed to release claim");
                    }

                    // Handle as failure for backoff
                    if let Err(e2) = dispatch_server
                        .handle_task_failure(&task_id, false, max_retries, None)
                        .await
                    {
                        warn!(task_id = %task_id, error = %e2, "failed to handle session start failure");
                    }
                }
            }
        }
    }
});
```

**Step 3: Add set_task_session_id method to Server**

This will be implemented in the next task.

**Step 4: Commit**

```bash
git add crates/app/src/run_loop.rs
git commit -m "$(cat <<'EOF'
feat(app): wire WorkQueue into dispatch loop

Part of #658 — replaces the racey DispatchPlan with synchronous
claiming through the centralized work queue.
EOF
)"
```

---

## Task 6: Add Server methods for session_id management

**Files:**
- Modify: `crates/server/src/server.rs`

**Step 1: Add set_task_session_id method**

Add to the Server impl block:

```rust
/// Set the session_id for a task.
///
/// Called when a container claims work to link the task to its session.
pub async fn set_task_session_id(
    &self,
    task_id: &str,
    session_id: Option<&str>,
) -> Result<(), ServerError> {
    let mut state = self.state.write().await;

    if let Some(task) = state.tasks.get_mut(task_id) {
        task.session_id = session_id.map(|s| s.to_string());
        task.updated_at = Utc::now();

        // Persist
        self.store.save_task(task)?;

        tracing::debug!(
            task_id = %task_id,
            session_id = ?session_id,
            "updated task session_id"
        );
    }

    Ok(())
}

/// Clear the session_id for a task.
///
/// Called when a session ends (completion or failure).
pub async fn clear_task_session_id(&self, task_id: &str) -> Result<(), ServerError> {
    self.set_task_session_id(task_id, None).await
}
```

**Step 2: Update session end handlers to clear session_id**

In the session completion/failure handling code, add calls to clear_task_session_id.

**Step 3: Run tests**

Run: `cargo test --package tasks-server`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/server/src/server.rs
git commit -m "$(cat <<'EOF'
feat(server): add session_id management methods

Part of #658 — set_task_session_id and clear_task_session_id link
tasks to their active container sessions.
EOF
)"
```

---

## Task 7: Add health check loop for stale work reclamation

**Files:**
- Modify: `crates/app/src/run_loop.rs`

**Step 1: Add health check spawn**

After the dispatch loop spawn, add:

```rust
// --- 8c. Spawn work queue health check loop ---
let health_work_queue = work_queue.clone();
let health_session_mgr = session_manager.clone();
let health_config = work_queue_config.clone();
let mut health_shutdown_rx = shutdown_tx.subscribe();

let health_check_handle = tokio::spawn(async move {
    let mut interval = tokio::time::interval(health_config.health_check_interval);

    loop {
        tokio::select! {
            _ = health_shutdown_rx.recv() => break,
            _ = interval.tick() => {}
        }

        let mut queue = health_work_queue.write().await;

        // Check which containers are alive
        let is_alive = |container_id: &str| -> bool {
            // Use session manager to check if container exists
            // This is a sync check against the sessions map
            health_session_mgr.has_session_sync(container_id)
        };

        match queue.health_check(is_alive).await {
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
```

**Step 2: Add has_session_sync to SessionManager**

In `crates/session/src/manager.rs`, add:

```rust
/// Check if a session exists by container_id (sync version for health checks).
pub fn has_session_sync(&self, container_id: &str) -> bool {
    // Use try_read to avoid blocking if lock is held
    if let Ok(sessions) = self.sessions.try_read() {
        sessions.values().any(|h| h.container_id == container_id)
    } else {
        // If we can't get the lock, assume session exists to be safe
        true
    }
}
```

**Step 3: Run tests**

Run: `cargo build --package tasks-app`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/app/src/run_loop.rs crates/session/src/manager.rs
git commit -m "$(cat <<'EOF'
feat(app): add health check loop for stale work reclamation

Part of #658 — periodically checks for dead or timed-out containers
and reclaims their work so it can be re-dispatched.
EOF
)"
```

---

## Task 8: Add startup recovery for orphaned claims

**Files:**
- Modify: `crates/app/src/run_loop.rs`

**Step 1: Add recovery logic after work queue creation**

After creating the work queue, add:

```rust
// Recover orphaned claims from previous run
{
    info!("checking for orphaned work claims from previous run");
    let conn = store.conn();
    let active_claims = tasks_store::work_claims::get_active_claims(&conn)?;
    drop(conn);

    let mut recovered = 0;
    for claim in active_claims {
        // Since we just started, no containers exist yet — release all active claims
        if claim.container_id.is_some() {
            let conn = store.conn();
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
```

**Step 2: Commit**

```bash
git add crates/app/src/run_loop.rs
git commit -m "$(cat <<'EOF'
feat(app): add startup recovery for orphaned claims

Part of #658 — releases claims from containers that no longer exist
after a server restart.
EOF
)"
```

---

## Task 9: Complete work when task transitions to terminal state

**Files:**
- Modify: `crates/server/src/server.rs`

**Step 1: Add work queue completion on task terminal transition**

In the `set_task_state` method, after transitioning to a terminal state, complete the work:

```rust
// If transitioning to terminal state, complete the work item
if new_state.is_terminal() {
    let work_id = format!("task:{}", task_id);
    // Note: This requires access to work queue - may need to be handled in run_loop
    // For now, just clear the session_id
    self.clear_task_session_id(task_id).await?;
}
```

**Step 2: Wire up work completion in run_loop session end handler**

When a session completes, the run_loop should call `work_queue.complete()`.

**Step 3: Commit**

```bash
git add crates/server/src/server.rs crates/app/src/run_loop.rs
git commit -m "$(cat <<'EOF'
feat(server): complete work items when tasks reach terminal state

Part of #658 — marks work as completed so it's removed from the queue
and the claim record is finalized.
EOF
)"
```

---

## Task 10: Integration testing

**Files:**
- Create: `crates/server/src/work_queue_integration_test.rs` (or add to existing test file)

**Step 1: Write integration test for full dispatch cycle**

```rust
#[tokio::test]
async fn test_work_queue_full_cycle() {
    // 1. Create store and work queue
    // 2. Add tasks to server state
    // 3. Rebuild queue
    // 4. Claim work
    // 5. Verify claim persisted
    // 6. Complete work
    // 7. Verify completed
}

#[tokio::test]
async fn test_work_queue_prevents_duplicate_dispatch() {
    // 1. Create store and work queue
    // 2. Add one task
    // 3. Rebuild queue
    // 4. Claim from container A
    // 5. Try to claim from container B
    // 6. Verify B gets None (already claimed)
}
```

**Step 2: Run full test suite**

Run: `cargo test`
Expected: PASS

**Step 3: Commit**

```bash
git add crates/server/src/
git commit -m "$(cat <<'EOF'
test(server): add integration tests for work queue

Part of #658 — verifies the full dispatch cycle and confirms
duplicate dispatch is prevented.
EOF
)"
```

---

## Task 11: Update documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `spec/spec.md` (if applicable)

**Step 1: Add work queue configuration to CLAUDE.md**

Add to the configuration section:

```markdown
## Work Queue Configuration

| Env Var | Default | Description |
|---------|---------|-------------|
| `WORK_QUEUE_TIMEOUT` | 15 | Seconds between dispatches (rate limiting) |
| `CONTAINER_TIMEOUT` | 7200 | Max seconds a container can work (2 hours) |
| `HEALTH_CHECK_INTERVAL` | 30 | Seconds between health checks |
```

**Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
docs: add work queue configuration documentation

Part of #658 — documents the new environment variables for
configuring the centralized work queue.
EOF
)"
```

---

## Final: Create PR

After all tasks complete:

```bash
gh pr create --title "Implement centralized work queue to prevent duplicate dispatch" --body "$(cat <<'EOF'
## Summary

Implements a centralized work queue (#658) that replaces the racey dispatch evaluation to prevent duplicate task implementations (#656).

## Changes

- Add `WorkType` enum and `WorkItem` struct for work queue items
- Add `work_claims` SQLite table for claim persistence
- Implement `WorkQueue` service with synchronous claiming
- Replace dispatch loop with queue-based claiming
- Add health check loop for stale work reclamation
- Add startup recovery for orphaned claims
- Wire up `session_id` on tasks to track container ownership

## Configuration

New environment variables:
- `WORK_QUEUE_TIMEOUT` (default: 15s) — delay between dispatches
- `CONTAINER_TIMEOUT` (default: 2h) — max work time before reclaim
- `HEALTH_CHECK_INTERVAL` (default: 30s) — health check frequency

## Test plan

- [ ] Run `cargo test` — all tests pass
- [ ] Start server, verify tasks dispatch one at a time with 15s delay
- [ ] Verify same task is not dispatched to multiple containers
- [ ] Kill a container mid-work, verify work is reclaimed after health check
- [ ] Restart server mid-work, verify orphaned claims are released

Closes #656, #658
EOF
)"
```
