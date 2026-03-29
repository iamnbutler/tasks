# Centralized Work Queue

**Issue:** [#656](https://github.com/iamnbutler/tasks/issues/656) — Duplicate task implementations
**Date:** 2026-03-29

## Problem

Multiple agents pick up the same task and create duplicate implementations. Root cause: dispatch evaluation can run concurrently, and there's no atomic claiming mechanism. The `session_id` field on tasks is never set during normal operations.

Race window:
```
T1: Dispatch A evaluates → selects task t1
T2: Dispatch B evaluates → also selects task t1 (still Waiting)
T3: Dispatch A → transitions t1 to Running + starts session
T4: Dispatch B → transitions t1 to Running (no-op) + starts ANOTHER session
Result: 2 sessions working on the same task
```

## Solution

A single canonical work queue that is the **only** path to dispatch work. Synchronous claiming eliminates races by design.

## Core Model

```rust
pub enum WorkType {
    MergeConflict,   // Highest priority
    PrFeedback,
    Automation,
    Task,
    // Future: OrchestratorRequest, etc.
}

pub struct WorkItem {
    pub id: String,           // Unique identifier
    pub work_type: WorkType,
    pub source_id: String,    // e.g., task_id, automation_id, pr_number
    pub project_id: String,
    pub priority: u32,        // Within-tier ordering (lower = higher priority)
    pub created_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub claimed_by: Option<String>,  // container_id
}
```

The queue is **derived** from source systems (GitHub, automations table, etc.) but **claim status is persisted** in SQLite so restarts don't re-dispatch in-progress work.

## Work Queue Service

```rust
pub struct WorkQueue {
    items: Vec<WorkItem>,           // Sorted by priority
    store: Arc<Store>,              // For claim persistence
    config: WorkQueueConfig,
    last_dispatch: Option<Instant>,
}

pub struct WorkQueueConfig {
    pub work_queue_timeout: Duration,      // 15s between dispatches
    pub container_timeout: Duration,       // 2h max work time
    pub health_check_interval: Duration,   // 30s between health checks
}

impl WorkQueue {
    /// Rebuilds queue from all sources, preserving claim status
    pub async fn rebuild(&mut self) -> Result<()>;

    /// Returns next claimable item (if any, respecting timeout + slots)
    pub async fn claim_next(&mut self, max_slots: usize, active_count: usize) -> Result<Option<WorkItem>>;

    /// Insert high-priority item (user request via #657)
    pub fn insert_priority(&mut self, item: WorkItem);

    /// Release a claim (container finished or relinquished)
    pub async fn release(&mut self, work_id: &str, note: Option<String>) -> Result<()>;

    /// Mark work as completed (removes from queue entirely)
    pub async fn complete(&mut self, work_id: &str) -> Result<()>;

    /// Health check - reclaim stale work from dead/timed-out containers
    pub async fn health_check(&mut self, session_manager: &SessionManager) -> Result<Vec<ReclaimedWork>>;
}
```

`claim_next()` enforces:
1. `WORK_QUEUE_TIMEOUT` (15s) since last dispatch
2. Available slots (`active_count < max_slots`)
3. Returns highest priority unclaimed item

## Dispatch Flow

```
┌─────────────────────────────────────────────────────────────┐
│                      Dispatch Loop                          │
│                                                             │
│  1. work_queue.rebuild()     ← Sync from GitHub, DB, etc.   │
│  2. work_queue.claim_next()  ← Returns next item (or None)  │
│  3. If item: create container, start session                │
│  4. Sleep briefly, repeat                                   │
│                                                             │
└─────────────────────────────────────────────────────────────┘

         ▲                           │
         │ release()/complete()      │ Container created with work_id
         │                           ▼

┌─────────────────────────────────────────────────────────────┐
│                      Container                              │
│  - Receives work_id + context at startup                    │
│  - Does work                                                │
│  - On completion: work_queue.complete(work_id)              │
│  - On failure/relinquish: work_queue.release(work_id, note) │
│  - Container terminates                                     │
└─────────────────────────────────────────────────────────────┘
```

Key change: **No more `DispatchPlan` with multiple candidates**. One item claimed at a time, synchronously. The timeout between dispatches prevents blitzing.

## Persistence

New SQLite table for claim status:

```sql
CREATE TABLE work_claims (
    work_id TEXT PRIMARY KEY,
    work_type TEXT NOT NULL,        -- 'merge_conflict', 'pr_feedback', 'automation', 'task'
    source_id TEXT NOT NULL,        -- References task.id, automation.id, etc.
    project_id TEXT NOT NULL,
    container_id TEXT,              -- NULL = unclaimed, set on claim
    claimed_at TIMESTAMP,
    released_at TIMESTAMP,          -- Set if relinquished (not completed)
    release_note TEXT,              -- Why container gave up the work
    completed_at TIMESTAMP
);

CREATE INDEX idx_work_claims_active ON work_claims(container_id) WHERE completed_at IS NULL;
```

On startup:
1. Load active claims (where `completed_at IS NULL AND container_id IS NOT NULL`)
2. Check if those containers still exist
3. If container gone but claim active → mark released (crashed container)
4. Rebuild queue from sources, filtering out completed work

## Container Health & Stale Work Reclamation

The work queue monitors claimed work and reclaims it when containers die or exceed time limits.

```rust
impl WorkQueue {
    pub async fn health_check(&mut self, session_manager: &SessionManager) -> Result<Vec<ReclaimedWork>> {
        let mut reclaimed = vec![];

        for claim in self.active_claims() {
            let should_reclaim = match session_manager.get_session(&claim.container_id) {
                None => true,  // Container gone
                Some(_session) => {
                    let elapsed = Utc::now() - claim.claimed_at;
                    elapsed > self.config.container_timeout  // Exceeded 2h
                }
            };

            if should_reclaim {
                self.release(&claim.work_id, Some("Container timeout or died")).await?;
                reclaimed.push(claim);
            }
        }

        reclaimed
    }
}
```

Reclaimed work goes back into the queue at its original priority.

## Integration & Migration

**What changes in existing code:**

1. **`crates/server/src/dispatcher.rs`** — Replaced by `WorkQueue::claim_next()`. The current `evaluate()` logic that builds `DispatchPlan` goes away.

2. **`crates/app/src/run_loop.rs`** — Dispatch loop simplifies to: rebuild → claim_next → create container. Remove multi-candidate logic.

3. **`crates/models/src/task.rs`** — `session_id` field finally gets used properly (set on claim, cleared on release/complete).

4. **`crates/session/src/manager.rs`** — Container creation receives `work_id`, passes it to supervisor so container knows what it's working on.

**New components:**

- `crates/server/src/work_queue.rs` — The `WorkQueue` struct and logic
- `crates/store/src/work_claims.rs` — SQLite operations for claims table
- Migration for `work_claims` table

## Configuration

```rust
// Environment variables with defaults
WORK_QUEUE_TIMEOUT=15        // seconds between dispatches
CONTAINER_TIMEOUT=7200       // seconds (2 hours) max work time
HEALTH_CHECK_INTERVAL=30     // seconds between health checks
```

## Future Work (TODOs)

- **#657**: User-requested task priority insertion via API endpoint
- **Container relinquishment**: Supervisor protocol message to release work with note
- **Orchestrator-driven priority**: Replace static WorkType ordering with AI-driven ranking
- **Container reuse**: Swap context instead of terminate/recreate
