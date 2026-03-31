//! Centralized work queue — the single source of truth for dispatchable work.
//!
//! This replaces the racey dispatch evaluation with synchronous claiming.
//! All work (tasks, automations, PR feedback, conflicts) flows through here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::model::task::{Task, TaskState};
use models::{ReclaimedWork, WorkItem, WorkType};
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
pub struct WorkQueue {
    items: Vec<WorkItem>,
    config: WorkQueueConfig,
    last_dispatch: Option<Instant>,
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
    /// Derives queue from tasks, preserves claim status from database.
    pub fn rebuild(&mut self, tasks: &HashMap<String, Task>) -> Result<(), WorkQueueError> {
        let mut items = Vec::new();

        // Collect tasks in dispatchable states
        for task in tasks.values() {
            let work_type = match task.state {
                TaskState::ChangesRequested => WorkType::PrFeedback,
                TaskState::Waiting => WorkType::Task,
                TaskState::Conflict => WorkType::MergeConflict,
                _ => continue,
            };

            // Check backoff for failed tasks
            if let Some(failure_at) = task.last_failure_at {
                let backoff = crate::dispatcher::backoff_duration(task.retry_count, &task.id);
                if failure_at + backoff > Utc::now() {
                    continue;
                }
            }

            let mut item = WorkItem::new(work_type, &task.id, &task.project);
            if let Some(p) = task.priority {
                item = item.with_priority(p as u32);
            }
            if let Some(created) = task.source_created_at {
                item = item.with_created_at(created);
            }
            items.push(item);
        }

        // Sort by: work_type tier, then priority, then created_at
        items.sort_by(|a, b| {
            a.work_type
                .cmp(&b.work_type)
                .then_with(|| a.priority.cmp(&b.priority))
                .then_with(|| a.created_at.cmp(&b.created_at))
        });

        // Load claim status from database
        let conn = self.store.conn()?;
        let active_claims = tasks_store::work_claims::get_active_claims(&conn)?;
        drop(conn);

        let claimed_sources: HashMap<String, String> = active_claims
            .iter()
            .filter_map(|c| {
                c.container_id
                    .as_ref()
                    .map(|cid| (c.source_id.clone(), cid.clone()))
            })
            .collect();

        for item in &mut items {
            if let Some(container_id) = claimed_sources.get(&item.source_id) {
                item.claimed_by = Some(container_id.clone());
                item.claimed_at = Some(Utc::now());
            }
        }

        self.items = items;
        debug!(count = self.items.len(), "work queue rebuilt");
        Ok(())
    }

    /// Claim the next available work item.
    /// Respects rate limit (work_queue_timeout) and slot limits.
    pub fn claim_next(
        &mut self,
        max_slots: usize,
        active_count: usize,
        container_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        if active_count >= max_slots {
            debug!(active = active_count, max = max_slots, "no slots available");
            return Ok(None);
        }

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

        let item_idx = self.items.iter().position(|item| !item.is_claimed());
        let Some(idx) = item_idx else {
            debug!("no unclaimed work items");
            return Ok(None);
        };

        let item = &mut self.items[idx];
        item.claimed_by = Some(container_id.to_string());
        item.claimed_at = Some(Utc::now());

        let conn = self.store.conn()?;
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

    /// Insert a high-priority work item at the front of its priority tier.
    pub fn insert_priority(&mut self, item: WorkItem) {
        // TODO(#657): Wire up user-requested task priority insertion via API endpoint
        // Find the first item of the same tier (to insert at front of tier)
        // or the first item of a lower tier (to append at end if no same-tier items)
        let insert_pos = self
            .items
            .iter()
            .position(|existing| existing.work_type >= item.work_type)
            .unwrap_or(self.items.len());
        self.items.insert(insert_pos, item);
    }

    /// Release a claim (container finished or relinquished).
    pub fn release(&mut self, work_id: &str, note: Option<&str>) -> Result<(), WorkQueueError> {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == work_id) {
            item.claimed_by = None;
            item.claimed_at = None;
        }

        let conn = self.store.conn()?;
        tasks_store::work_claims::release_claim(&conn, work_id, note)?;
        drop(conn);

        info!(work_id = %work_id, note = ?note, "work item released");
        Ok(())
    }

    /// Mark work as completed (removes from queue).
    pub fn complete(&mut self, work_id: &str) -> Result<(), WorkQueueError> {
        self.items.retain(|i| i.id != work_id);

        let conn = self.store.conn()?;
        tasks_store::work_claims::complete_claim(&conn, work_id)?;
        drop(conn);

        info!(work_id = %work_id, "work item completed");
        Ok(())
    }

    /// Health check — reclaim work from dead/timed-out sessions.
    ///
    /// Uses `source_id` (task_id) to check if a session is alive, not `container_id`.
    /// This avoids a bug where the work queue's container_id doesn't match the
    /// runtime's actual container_id.
    pub fn health_check<F>(
        &mut self,
        is_session_alive: F,
    ) -> Result<Vec<ReclaimedWork>, WorkQueueError>
    where
        F: Fn(&str) -> bool,
    {
        let mut reclaimed = Vec::new();

        let conn = self.store.conn()?;
        let active_claims = tasks_store::work_claims::get_active_claims(&conn)?;
        drop(conn);

        for claim in active_claims {
            let Some(container_id) = &claim.container_id else {
                continue;
            };

            // Check if session is alive using source_id (task_id), not container_id.
            // The work queue's container_id is a pre-generated UUID that doesn't match
            // the actual container ID from the runtime.
            let should_reclaim = if !is_session_alive(&claim.source_id) {
                Some("session not found".to_string())
            } else if let Some(claimed_at) = claim.claimed_at {
                let elapsed = Utc::now().signed_duration_since(claimed_at);
                if elapsed.num_seconds() > self.config.container_timeout.as_secs() as i64 {
                    Some(format!(
                        "exceeded {}h timeout",
                        self.config.container_timeout.as_secs() / 3600
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(reason) = should_reclaim {
                warn!(work_id = %claim.work_id, container_id = %container_id, reason = %reason, "reclaiming work");
                self.release(&claim.work_id, Some(&reason))?;
                reclaimed.push(ReclaimedWork {
                    work_id: claim.work_id,
                    previous_container_id: container_id.clone(),
                    reason,
                });
            }
        }

        Ok(reclaimed)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn unclaimed_count(&self) -> usize {
        self.items.iter().filter(|i| !i.is_claimed()).count()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkQueueError {
    #[error("store error: {0}")]
    Store(#[from] tasks_store::StoreError),
}

// TODO: Container task relinquishment - supervisor protocol message to release work with note

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::task::TaskSource;
    use chrono::Duration as ChronoDuration;

    fn setup_store() -> Arc<Store> {
        Arc::new(Store::open_memory().expect("failed to create in-memory store"))
    }

    fn make_task(id: &str, project: &str, state: TaskState) -> Task {
        let mut task = Task::new(id, TaskSource::Internal, id, project);
        task.state = state;
        task
    }

    fn make_tasks(specs: &[(&str, &str, TaskState)]) -> HashMap<String, Task> {
        specs
            .iter()
            .map(|(id, project, state)| (id.to_string(), make_task(id, project, *state)))
            .collect()
    }

    // ---- rebuild tests ----

    #[test]
    fn rebuild_collects_waiting_tasks() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::Waiting),
            ("t2", "proj", TaskState::Waiting),
            ("t3", "proj", TaskState::Running), // not collected
        ]);

        queue.rebuild(&tasks).unwrap();

        assert_eq!(queue.len(), 2);
        assert!(queue.items.iter().any(|i| i.source_id == "t1"));
        assert!(queue.items.iter().any(|i| i.source_id == "t2"));
    }

    #[test]
    fn rebuild_collects_changes_requested_tasks() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::ChangesRequested),
            ("t2", "proj", TaskState::Waiting),
        ]);

        queue.rebuild(&tasks).unwrap();

        assert_eq!(queue.len(), 2);
        let pr_feedback = queue
            .items
            .iter()
            .find(|i| i.source_id == "t1")
            .unwrap();
        assert_eq!(pr_feedback.work_type, WorkType::PrFeedback);
    }

    #[test]
    fn rebuild_collects_conflict_tasks() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[("t1", "proj", TaskState::Conflict)]);

        queue.rebuild(&tasks).unwrap();

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.items[0].work_type, WorkType::MergeConflict);
    }

    #[test]
    fn rebuild_excludes_terminal_tasks() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::Completed),
            ("t2", "proj", TaskState::Failed),
            ("t3", "proj", TaskState::Cancelled),
        ]);

        queue.rebuild(&tasks).unwrap();

        assert!(queue.is_empty());
    }

    #[test]
    fn rebuild_excludes_active_tasks() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::Running),
            ("t2", "proj", TaskState::Question),
            ("t3", "proj", TaskState::Testing),
        ]);

        queue.rebuild(&tasks).unwrap();

        assert!(queue.is_empty());
    }

    // ---- claim_next tests ----

    #[test]
    fn claim_next_respects_slots() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0), // disable rate limit
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store, config);

        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // At capacity - no claim
        let result = queue.claim_next(2, 2, "container-1").unwrap();
        assert!(result.is_none());

        // Has room - can claim
        let result = queue.claim_next(2, 1, "container-1").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn claim_next_respects_rate_limit() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(60), // long timeout
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store, config);

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::Waiting),
            ("t2", "proj", TaskState::Waiting),
        ]);
        queue.rebuild(&tasks).unwrap();

        // First claim should work
        let result = queue.claim_next(10, 0, "container-1").unwrap();
        assert!(result.is_some());

        // Second claim should be rate-limited
        let result = queue.claim_next(10, 1, "container-2").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn claim_next_returns_none_when_all_claimed() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store, config);

        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // First claim succeeds
        let result = queue.claim_next(10, 0, "container-1").unwrap();
        assert!(result.is_some());

        // Second claim fails (all claimed)
        let result = queue.claim_next(10, 1, "container-2").unwrap();
        assert!(result.is_none());
    }

    // ---- priority ordering tests ----

    #[test]
    fn priority_ordering_changes_requested_before_waiting() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::Waiting),
            ("t2", "proj", TaskState::ChangesRequested),
        ]);
        queue.rebuild(&tasks).unwrap();

        // ChangesRequested (PrFeedback) should come before Waiting (Task)
        assert_eq!(queue.items[0].work_type, WorkType::PrFeedback);
        assert_eq!(queue.items[1].work_type, WorkType::Task);
    }

    #[test]
    fn priority_ordering_conflict_before_changes_requested() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::ChangesRequested),
            ("t2", "proj", TaskState::Conflict),
            ("t3", "proj", TaskState::Waiting),
        ]);
        queue.rebuild(&tasks).unwrap();

        // MergeConflict < PrFeedback < Task
        assert_eq!(queue.items[0].work_type, WorkType::MergeConflict);
        assert_eq!(queue.items[1].work_type, WorkType::PrFeedback);
        assert_eq!(queue.items[2].work_type, WorkType::Task);
    }

    #[test]
    fn priority_ordering_by_task_priority() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let mut tasks = make_tasks(&[
            ("t1", "proj", TaskState::Waiting),
            ("t2", "proj", TaskState::Waiting),
        ]);
        tasks.get_mut("t1").unwrap().priority = Some(10);
        tasks.get_mut("t2").unwrap().priority = Some(1);

        queue.rebuild(&tasks).unwrap();

        // Lower priority number comes first
        assert_eq!(queue.items[0].source_id, "t2");
        assert_eq!(queue.items[1].source_id, "t1");
    }

    #[test]
    fn priority_ordering_by_created_at() {
        let store = setup_store();
        let mut queue = WorkQueue::new(store, WorkQueueConfig::default());

        let now = Utc::now();
        let mut tasks = make_tasks(&[
            ("t1", "proj", TaskState::Waiting),
            ("t2", "proj", TaskState::Waiting),
        ]);
        tasks.get_mut("t1").unwrap().source_created_at = Some(now);
        tasks.get_mut("t2").unwrap().source_created_at = Some(now - ChronoDuration::hours(1));

        queue.rebuild(&tasks).unwrap();

        // Older task comes first
        assert_eq!(queue.items[0].source_id, "t2");
        assert_eq!(queue.items[1].source_id, "t1");
    }

    // ---- complete tests ----

    #[test]
    fn complete_removes_from_queue() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store, config);

        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        assert_eq!(queue.len(), 1);

        queue.complete("task:t1").unwrap();

        assert!(queue.is_empty());
    }

    // ---- release tests ----

    #[test]
    fn release_makes_item_available() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store, config);

        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // Claim the item
        let claimed = queue.claim_next(10, 0, "container-1").unwrap().unwrap();
        assert_eq!(queue.unclaimed_count(), 0);

        // Release it
        queue.release(&claimed.id, Some("testing")).unwrap();
        assert_eq!(queue.unclaimed_count(), 1);
    }

    // ---- health_check tests ----

    #[test]
    fn health_check_reclaims_from_dead_sessions() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store.clone(), config);

        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // Claim the item
        queue.claim_next(10, 0, "container-1").unwrap();

        // Health check with dead session (closure receives source_id, not container_id)
        let reclaimed = queue.health_check(|_| false).unwrap();

        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].previous_container_id, "container-1");
        assert_eq!(queue.unclaimed_count(), 1);
    }

    #[test]
    fn health_check_keeps_alive_sessions() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store.clone(), config);

        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // Claim the item
        queue.claim_next(10, 0, "container-1").unwrap();

        // Health check with alive session (closure receives source_id, not container_id)
        let reclaimed = queue.health_check(|_| true).unwrap();

        assert!(reclaimed.is_empty());
        assert_eq!(queue.unclaimed_count(), 0);
    }

    #[test]
    fn health_check_passes_source_id_not_container_id() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store.clone(), config);

        // Task with source_id "t1"
        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // Claim with container_id "container-xyz"
        queue.claim_next(10, 0, "container-xyz").unwrap();

        // Health check should pass source_id "t1", NOT container_id "container-xyz".
        // We verify this by returning true for "t1" (alive) and false for anything else.
        // If container_id was passed, it would be "container-xyz" which would return false
        // and the task would be reclaimed.
        let reclaimed = queue.health_check(|id| id == "t1").unwrap();

        // If source_id is passed, closure returns true for "t1", so nothing is reclaimed.
        // If container_id "container-xyz" was passed, it would return false and reclaim.
        assert!(
            reclaimed.is_empty(),
            "task should not be reclaimed because closure receives source_id 't1' (which returns true), \
             not container_id 'container-xyz' (which would return false)"
        );
    }

    // ---- unclaimed_count tests ----

    #[test]
    fn unclaimed_count_reflects_queue_state() {
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store, config);

        let tasks = make_tasks(&[
            ("t1", "proj", TaskState::Waiting),
            ("t2", "proj", TaskState::Waiting),
        ]);
        queue.rebuild(&tasks).unwrap();

        assert_eq!(queue.unclaimed_count(), 2);

        queue.claim_next(10, 0, "container-1").unwrap();
        assert_eq!(queue.unclaimed_count(), 1);
    }

    // ---- integration tests (database verification) ----

    #[test]
    fn full_dispatch_cycle_with_database_verification() {
        // This test verifies the full lifecycle: claim persisted to DB, complete persisted to DB
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store.clone(), config);

        // 1. Add tasks via rebuild
        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();
        assert_eq!(queue.len(), 1);

        // 2. Claim work
        let claimed = queue.claim_next(10, 0, "container-1").unwrap().unwrap();
        assert_eq!(claimed.source_id, "t1");

        // 3. Verify claim persisted in database
        let conn = store.conn().unwrap();
        let db_claim = tasks_store::work_claims::get_claim(&conn, &claimed.id)
            .unwrap()
            .expect("claim should exist in database");
        assert_eq!(db_claim.container_id, Some("container-1".to_string()));
        assert!(db_claim.claimed_at.is_some());
        assert!(db_claim.completed_at.is_none());
        drop(conn);

        // 4. Complete work
        queue.complete(&claimed.id).unwrap();

        // 5. Verify completed in database
        let conn = store.conn().unwrap();
        let db_claim = tasks_store::work_claims::get_claim(&conn, &claimed.id)
            .unwrap()
            .expect("claim should still exist after completion");
        assert!(db_claim.completed_at.is_some());

        // 6. Verify removed from queue
        assert!(queue.is_empty());
    }

    #[test]
    fn duplicate_prevention_across_containers() {
        // Verifies that once container A claims work, container B cannot claim it
        let store = setup_store();
        let config = WorkQueueConfig {
            work_queue_timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let mut queue = WorkQueue::new(store.clone(), config);

        // Add one task
        let tasks = make_tasks(&[("t1", "proj", TaskState::Waiting)]);
        queue.rebuild(&tasks).unwrap();

        // Container A claims the work
        let claimed = queue.claim_next(10, 0, "container-A").unwrap().unwrap();
        assert_eq!(claimed.source_id, "t1");

        // Verify database has container A's claim
        let conn = store.conn().unwrap();
        let db_claim = tasks_store::work_claims::get_claim(&conn, &claimed.id)
            .unwrap()
            .expect("claim should exist");
        assert_eq!(db_claim.container_id, Some("container-A".to_string()));
        drop(conn);

        // Container B tries to claim - should get None (already claimed)
        let result = queue.claim_next(10, 1, "container-B").unwrap();
        assert!(result.is_none(), "container B should not get work already claimed by A");

        // Verify database still shows container A's claim (not overwritten)
        let conn = store.conn().unwrap();
        let db_claim = tasks_store::work_claims::get_claim(&conn, &claimed.id)
            .unwrap()
            .expect("claim should still exist");
        assert_eq!(
            db_claim.container_id,
            Some("container-A".to_string()),
            "claim should still belong to container A"
        );
    }
}
