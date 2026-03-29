//! Dispatch evaluation — spec §13 (Scheduling and Dispatch).
//!
//! Pure logic for selecting and prioritizing task candidates.
//! No async, no sessions — just candidate selection and sorting.

use std::collections::HashMap;

use chrono::{Duration, Utc};

use crate::model::task::{Task, TaskState};

/// Result of a dispatch evaluation.
#[derive(Debug)]
pub struct DispatchPlan {
    /// Task IDs in question state with pending answers — resume immediately.
    /// These are free (no slot cost).
    pub resume: Vec<String>,
    /// Task IDs in waiting state — start new sessions, in priority order.
    /// Limited by concurrency constraints.
    pub new_work: Vec<String>,
}

/// Calculate deterministic jitter factor based on task ID and retry count.
/// Returns a value in the range [-0.25, +0.25].
fn jitter_factor(task_id: &str, retry_count: u32) -> f64 {
    // djb2-like hash combining task_id and retry_count
    let mut h: u64 = 5381u64.wrapping_add(retry_count as u64 * 2654435761);
    for b in task_id.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u64);
    }
    // Map h to [-0.25, +0.25]
    // h % 1001 gives [0, 1000], divide by 2000 gives [0, 0.5], subtract 0.25 gives [-0.25, 0.25]
    ((h % 1001) as f64 / 2000.0) - 0.25
}

/// Calculate backoff duration for a given retry count (spec §14.2).
/// Base: 5s, multiplier: 2x, max: 300s, jitter: ±25%.
///
/// Jitter is computed deterministically from the task ID to prevent
/// thundering herd when multiple tasks fail simultaneously, while
/// remaining predictable across dispatch evaluations.
pub fn backoff_duration(retry_count: u32, task_id: &str) -> Duration {
    let base_secs = 5i64 * 2i64.pow(retry_count.min(6));
    let capped_secs = base_secs.min(300);

    // Apply ±25% jitter
    let jitter = jitter_factor(task_id, retry_count);
    let jittered_secs = (capped_secs as f64 * (1.0 + jitter)).round() as i64;

    // Clamp to [5, 300] per spec §13.2
    let final_secs = jittered_secs.clamp(5, 300);

    Duration::seconds(final_secs)
}

/// Evaluate which tasks should be dispatched (spec §13.6).
///
/// Takes current tasks, per-project session limits, and global max.
/// Returns a DispatchPlan with resume candidates and new work in priority order.
///
/// `pending_answers` is the set of task IDs in Question state that have
/// received a message (human or orchestrator). These are resume candidates.
///
/// `tasks_with_active_prs` is the set of task IDs that have an unclosed PR
/// in the merge queue. These tasks are skipped since work is already in progress.
pub fn evaluate(
    tasks: &HashMap<String, Task>,
    pending_answers: &[String],
    tasks_with_active_prs: &std::collections::HashSet<String>,
    project_limits: &HashMap<String, u32>,
    global_max: u32,
) -> DispatchPlan {
    let now = Utc::now();

    // 1. Resume candidates: tasks in Question state whose ID is in pending_answers.
    let resume: Vec<String> = pending_answers
        .iter()
        .filter(|id| {
            tasks
                .get(id.as_str())
                .is_some_and(|t| t.state == TaskState::Question)
        })
        .cloned()
        .collect();

    // 2. Count active slots (Running, Question, Testing).
    let mut global_active: u32 = 0;
    let mut project_active: HashMap<String, u32> = HashMap::new();
    for task in tasks.values() {
        if matches!(
            task.state,
            TaskState::Running | TaskState::Question | TaskState::Testing
        ) {
            global_active += 1;
            *project_active.entry(task.project.clone()).or_insert(0) += 1;
        }
    }

    // 3. Collect new work candidates: Waiting or ChangesRequested tasks with
    //    backoff elapsed and (for Waiting only) no active PR in merge queue.
    //    ChangesRequested tasks supersede Waiting tasks per spec §7.1.
    let mut candidates: Vec<&Task> = tasks
        .values()
        .filter(|t| {
            // Accept Waiting or ChangesRequested tasks
            let is_waiting = t.state == TaskState::Waiting;
            let is_changes_requested = t.state == TaskState::ChangesRequested;
            if !is_waiting && !is_changes_requested {
                return false;
            }
            // Skip Waiting tasks that already have an unclosed PR in the merge queue
            // (ChangesRequested tasks DO have a PR, so skip this check for them)
            if is_waiting && tasks_with_active_prs.contains(&t.id) {
                return false;
            }
            // Check backoff
            if let Some(failure_at) = t.last_failure_at {
                if failure_at + backoff_duration(t.retry_count, &t.id) > now {
                    return false;
                }
            }
            true
        })
        .collect();

    // 4. Build unblocking value set: task IDs that appear in any task's blocked_by.
    let mut unblocking_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for task in tasks.values() {
        for blocked_by_id in &task.blocked_by {
            unblocking_ids.insert(blocked_by_id.as_str());
        }
    }

    // 5. Sort candidates by priority rules (spec §13.3).
    candidates.sort_by(|a, b| {
        // ChangesRequested tasks supersede Waiting tasks (spec §7.1).
        // This ensures work that needs minor fixes gets addressed before new work.
        let cr_a = a.state == TaskState::ChangesRequested;
        let cr_b = b.state == TaskState::ChangesRequested;
        let cr_cmp = cr_b.cmp(&cr_a); // true before false
        if cr_cmp != std::cmp::Ordering::Equal {
            return cr_cmp;
        }

        // Explicit priority: lower number first, None sorts last.
        let pri_a = a.priority.unwrap_or(i32::MAX);
        let pri_b = b.priority.unwrap_or(i32::MAX);
        let pri_cmp = pri_a.cmp(&pri_b);
        if pri_cmp != std::cmp::Ordering::Equal {
            return pri_cmp;
        }

        // Unblocking value: tasks that unblock others sort first.
        let unblock_a = unblocking_ids.contains(a.id.as_str());
        let unblock_b = unblocking_ids.contains(b.id.as_str());
        let unblock_cmp = unblock_b.cmp(&unblock_a); // true before false
        if unblock_cmp != std::cmp::Ordering::Equal {
            return unblock_cmp;
        }

        // Retry count: fewer retries first (deprioritize repeatedly-failed tasks).
        let retry_cmp = a.retry_count.cmp(&b.retry_count);
        if retry_cmp != std::cmp::Ordering::Equal {
            return retry_cmp;
        }

        // Source number: lower issue/PR numbers first (ascending), but only
        // compared within the same project. Issue numbers are per-repo, so
        // comparing across projects has no semantic meaning.
        if a.project == b.project {
            let num_a = a.source_number.unwrap_or(u64::MAX);
            let num_b = b.source_number.unwrap_or(u64::MAX);
            let num_cmp = num_a.cmp(&num_b);
            if num_cmp != std::cmp::Ordering::Equal {
                return num_cmp;
            }
        }

        // Fallback: older creation date first (ascending).
        // For tasks with the same source number (shouldn't happen) or
        // both without source numbers, prefer older tasks.
        let time_a = a.source_created_at.unwrap_or(a.created_at);
        let time_b = b.source_created_at.unwrap_or(b.created_at);
        time_a.cmp(&time_b)
    });

    // 6. Apply concurrency limits.
    let mut new_work: Vec<String> = Vec::new();
    let mut dispatched_global: u32 = 0;
    let mut dispatched_project: HashMap<String, u32> = HashMap::new();

    for candidate in &candidates {
        // Check global limit.
        if global_active + dispatched_global >= global_max {
            break;
        }

        // Check per-project limit.
        let project = &candidate.project;
        let project_limit = project_limits
            .get(project)
            .copied()
            .unwrap_or(global_max);
        let project_current = project_active.get(project).copied().unwrap_or(0);
        let project_dispatched = dispatched_project.get(project).copied().unwrap_or(0);

        if project_current + project_dispatched >= project_limit {
            // Skip this candidate but continue checking others from different projects.
            continue;
        }

        new_work.push(candidate.id.clone());
        dispatched_global += 1;
        *dispatched_project.entry(project.clone()).or_insert(0) += 1;
    }

    DispatchPlan { resume, new_work }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::task::{Task, TaskSource, TaskState};
    use chrono::{Duration, Utc};
    use std::collections::{HashMap, HashSet};

    /// Helper to create a minimal task in Waiting state.
    fn make_task(id: &str, project: &str) -> Task {
        Task::new(id.to_string(), TaskSource::Internal, id, project.to_string())
    }

    /// Helper to insert a task into a HashMap.
    fn insert(map: &mut HashMap<String, Task>, task: Task) {
        map.insert(task.id.clone(), task);
    }

    /// Helper for an empty active PRs set.
    fn no_active_prs() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn empty_tasks_returns_empty_plan() {
        let tasks = HashMap::new();
        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert!(plan.resume.is_empty());
        assert!(plan.new_work.is_empty());
    }

    #[test]
    fn resume_candidates_from_pending_answers() {
        let mut tasks = HashMap::new();
        let mut t = make_task("t1", "proj");
        t.state = TaskState::Question;
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &["t1".to_string()], &no_active_prs(), &HashMap::new(), 10);
        assert_eq!(plan.resume, vec!["t1"]);
    }

    #[test]
    fn question_without_answer_not_resumed() {
        let mut tasks = HashMap::new();
        let mut t = make_task("t1", "proj");
        t.state = TaskState::Question;
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert!(plan.resume.is_empty());
    }

    #[test]
    fn waiting_task_becomes_new_work() {
        let mut tasks = HashMap::new();
        let t = make_task("t1", "proj");
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert_eq!(plan.new_work, vec!["t1"]);
    }

    #[test]
    fn priority_sorting_explicit_priority() {
        let mut tasks = HashMap::new();

        let mut t1 = make_task("t1", "proj");
        t1.priority = Some(5);
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(1);
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert_eq!(plan.new_work, vec!["t2", "t1"]);
    }

    #[test]
    fn priority_sorting_null_priority_last() {
        let mut tasks = HashMap::new();

        let mut t1 = make_task("t1", "proj");
        t1.priority = None;
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(3);
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert_eq!(plan.new_work, vec!["t2", "t1"]);
    }

    #[test]
    fn priority_sorting_unblocking_value() {
        let mut tasks = HashMap::new();

        // t1 and t2 have the same priority. t1 unblocks t3, so t1 should sort first.
        let mut t1 = make_task("t1", "proj");
        t1.priority = Some(2);
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(2);
        insert(&mut tasks, t2);

        let mut t3 = make_task("t3", "proj");
        t3.state = TaskState::Blocked;
        t3.blocked_by = vec!["t1".to_string()];
        insert(&mut tasks, t3);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // t1 unblocks t3, so t1 before t2. t3 is Blocked, not in new_work.
        assert_eq!(plan.new_work.len(), 2);
        assert_eq!(plan.new_work[0], "t1");
        assert_eq!(plan.new_work[1], "t2");
    }

    #[test]
    fn priority_sorting_by_source_number() {
        let mut tasks = HashMap::new();

        // t1 has lower issue number (older), should sort first
        let mut t1 = make_task("t1", "proj");
        t1.priority = Some(1);
        t1.source_number = Some(10);
        insert(&mut tasks, t1);

        // t2 has higher issue number (newer), should sort second
        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(1);
        t2.source_number = Some(15);
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // Lower source_number (older issue) sorts first.
        assert_eq!(plan.new_work, vec!["t1", "t2"]);
    }

    #[test]
    fn priority_sorting_fallback_to_created_at() {
        let mut tasks = HashMap::new();

        let now = Utc::now();

        // Both tasks have no source_number (internal tasks), so fall back to created_at.
        let mut t1 = make_task("t1", "proj");
        t1.priority = Some(1);
        t1.created_at = now - Duration::seconds(100);
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(1);
        t2.created_at = now;
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // Without source_number, older created_at sorts first.
        assert_eq!(plan.new_work, vec!["t1", "t2"]);
    }

    #[test]
    fn global_concurrency_limit() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 1);
        assert_eq!(plan.new_work.len(), 1);
    }

    #[test]
    fn per_project_concurrency_limit() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        let mut project_limits = HashMap::new();
        project_limits.insert("proj".to_string(), 1);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &project_limits, 10);
        assert_eq!(plan.new_work.len(), 1);
    }

    #[test]
    fn running_tasks_count_as_slots() {
        let mut tasks = HashMap::new();

        let mut running = make_task("r1", "proj");
        running.state = TaskState::Running;
        insert(&mut tasks, running);

        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 2);
        // 1 Running occupies a slot, so only 1 new slot available.
        assert_eq!(plan.new_work.len(), 1);
    }

    #[test]
    fn backoff_excludes_recent_failures() {
        let mut tasks = HashMap::new();

        let mut t = make_task("t1", "proj");
        t.retry_count = 1;
        t.last_failure_at = Some(Utc::now()); // just failed
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // Backoff for retry_count=1 is 10s, so this task should be excluded.
        assert!(plan.new_work.is_empty());
    }

    #[test]
    fn terminal_tasks_not_candidates() {
        let mut tasks = HashMap::new();

        let mut t1 = make_task("t1", "proj");
        t1.state = TaskState::Completed;
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.state = TaskState::Failed;
        insert(&mut tasks, t2);

        let mut t3 = make_task("t3", "proj");
        t3.state = TaskState::Cancelled;
        insert(&mut tasks, t3);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert!(plan.new_work.is_empty());
    }

    #[test]
    fn backoff_duration_within_jitter_range() {
        // Test that backoff durations fall within expected jitter range (±25%)
        let task_id = "test-task";

        // retry 0: base 5s, range [4, 6] (clamped to [5, 6])
        let d0 = backoff_duration(0, task_id).num_seconds();
        assert!(d0 >= 5 && d0 <= 6, "retry 0: expected [5,6], got {}", d0);

        // retry 1: base 10s, range [8, 13]
        let d1 = backoff_duration(1, task_id).num_seconds();
        assert!(d1 >= 8 && d1 <= 13, "retry 1: expected [8,13], got {}", d1);

        // retry 2: base 20s, range [15, 25]
        let d2 = backoff_duration(2, task_id).num_seconds();
        assert!(d2 >= 15 && d2 <= 25, "retry 2: expected [15,25], got {}", d2);

        // retry 6: base 320s capped to 300s, range [225, 300] (clamped at 300)
        let d6 = backoff_duration(6, task_id).num_seconds();
        assert!(d6 >= 225 && d6 <= 300, "retry 6: expected [225,300], got {}", d6);

        // retry 10: same as retry 6 due to min(6)
        let d10 = backoff_duration(10, task_id).num_seconds();
        assert!(d10 >= 225 && d10 <= 300, "retry 10: expected [225,300], got {}", d10);
    }

    #[test]
    fn backoff_jitter_is_deterministic() {
        // Same task_id and retry_count should produce same duration
        let d1 = backoff_duration(2, "task-abc");
        let d2 = backoff_duration(2, "task-abc");
        assert_eq!(d1, d2);
    }

    #[test]
    fn backoff_jitter_varies_by_task_id() {
        // Different task_ids should (usually) produce different durations
        // Use a higher retry count for more variation
        let d1 = backoff_duration(3, "task-alpha").num_seconds();
        let d2 = backoff_duration(3, "task-beta").num_seconds();
        let d3 = backoff_duration(3, "task-gamma").num_seconds();

        // With 3 different task IDs, we should get at least 2 different values
        let unique_count = [d1, d2, d3]
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(unique_count >= 2, "Expected variation in jitter, got {} {} {}", d1, d2, d3);
    }

    #[test]
    fn backoff_jitter_varies_by_retry_count() {
        // Same task_id with different retry counts should produce different jitter factors
        let task_id = "fixed-task";
        let d1 = backoff_duration(1, task_id).num_seconds();
        let d2 = backoff_duration(2, task_id).num_seconds();

        // Base values are 10s and 20s, so even with jitter they shouldn't be equal
        assert_ne!(d1, d2);
    }

    #[test]
    fn elapsed_backoff_allows_dispatch() {
        let mut tasks = HashMap::new();

        let mut t = make_task("t1", "proj");
        t.retry_count = 1;
        // Failed 20 seconds ago; backoff for retry_count=1 is 10s, so it should be allowed.
        t.last_failure_at = Some(Utc::now() - Duration::seconds(20));
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        assert_eq!(plan.new_work, vec!["t1"]);
    }

    #[test]
    fn per_project_limit_allows_other_projects() {
        let mut tasks = HashMap::new();

        // Two tasks in project A, one in project B.
        insert(&mut tasks, make_task("a1", "projA"));
        insert(&mut tasks, make_task("a2", "projA"));
        insert(&mut tasks, make_task("b1", "projB"));

        let mut project_limits = HashMap::new();
        project_limits.insert("projA".to_string(), 1);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &project_limits, 10);
        // projA limited to 1, projB unlimited → 1 from A + 1 from B = 2.
        assert_eq!(plan.new_work.len(), 2);
        // b1 should be in there.
        assert!(plan.new_work.contains(&"b1".to_string()));
        // Exactly one of a1/a2 should be in there.
        let a_count = plan
            .new_work
            .iter()
            .filter(|id| id.starts_with('a'))
            .count();
        assert_eq!(a_count, 1);
    }

    #[test]
    fn resume_does_not_consume_slots() {
        let mut tasks = HashMap::new();

        // One task in Question state (occupies a slot already).
        let mut q = make_task("q1", "proj");
        q.state = TaskState::Question;
        insert(&mut tasks, q);

        // One waiting task.
        insert(&mut tasks, make_task("t1", "proj"));

        // global_max=2: q1 uses 1 slot, so t1 should get the other.
        let plan = evaluate(
            &tasks,
            &["q1".to_string()],
            &no_active_prs(),
            &HashMap::new(),
            2,
        );
        assert_eq!(plan.resume, vec!["q1"]);
        assert_eq!(plan.new_work, vec!["t1"]);
    }

    #[test]
    fn task_with_active_pr_skipped() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        // t1 has an active PR in the merge queue
        let mut active_prs = HashSet::new();
        active_prs.insert("t1".to_string());

        let plan = evaluate(&tasks, &[], &active_prs, &HashMap::new(), 10);
        // Only t2 should be in new_work, t1 is skipped
        assert_eq!(plan.new_work, vec!["t2"]);
    }

    #[test]
    fn multiple_tasks_with_active_prs_skipped() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));
        insert(&mut tasks, make_task("t3", "proj"));

        // t1 and t2 have active PRs
        let mut active_prs = HashSet::new();
        active_prs.insert("t1".to_string());
        active_prs.insert("t2".to_string());

        let plan = evaluate(&tasks, &[], &active_prs, &HashMap::new(), 10);
        // Only t3 should be in new_work
        assert_eq!(plan.new_work, vec!["t3"]);
    }

    #[test]
    fn all_tasks_with_active_prs_returns_empty() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        // Both tasks have active PRs
        let mut active_prs = HashSet::new();
        active_prs.insert("t1".to_string());
        active_prs.insert("t2".to_string());

        let plan = evaluate(&tasks, &[], &active_prs, &HashMap::new(), 10);
        assert!(plan.new_work.is_empty());
    }

    #[test]
    fn changes_requested_tasks_supersede_waiting() {
        let mut tasks = HashMap::new();

        // t1 is Waiting with high priority
        let mut t1 = make_task("t1", "proj");
        t1.state = TaskState::Waiting;
        t1.priority = Some(1); // highest priority
        insert(&mut tasks, t1);

        // t2 is ChangesRequested with low priority
        let mut t2 = make_task("t2", "proj");
        t2.state = TaskState::ChangesRequested;
        t2.priority = Some(100); // low priority
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // ChangesRequested should come first regardless of priority
        assert_eq!(plan.new_work.len(), 2);
        assert_eq!(plan.new_work[0], "t2"); // ChangesRequested first
        assert_eq!(plan.new_work[1], "t1"); // Waiting second
    }

    #[test]
    fn changes_requested_tasks_sorted_by_priority() {
        let mut tasks = HashMap::new();

        // Both are ChangesRequested, different priorities
        let mut t1 = make_task("t1", "proj");
        t1.state = TaskState::ChangesRequested;
        t1.priority = Some(10);
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.state = TaskState::ChangesRequested;
        t2.priority = Some(1);
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // Both ChangesRequested, so sort by priority
        assert_eq!(plan.new_work.len(), 2);
        assert_eq!(plan.new_work[0], "t2"); // priority 1
        assert_eq!(plan.new_work[1], "t1"); // priority 10
    }

    #[test]
    fn changes_requested_counts_as_slot() {
        let mut tasks = HashMap::new();

        // t1 is ChangesRequested (should dispatch and consume a slot)
        let mut t1 = make_task("t1", "proj");
        t1.state = TaskState::ChangesRequested;
        insert(&mut tasks, t1);

        // t2 is Waiting
        insert(&mut tasks, make_task("t2", "proj"));

        // global_max=1: only one task should dispatch
        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 1);
        assert_eq!(plan.new_work.len(), 1);
        assert_eq!(plan.new_work[0], "t1"); // ChangesRequested gets priority
    }

    #[test]
    fn retry_count_deprioritizes_failed_tasks() {
        let mut tasks = HashMap::new();

        // t1 has failed twice (higher retry count)
        let mut t1 = make_task("t1", "proj");
        t1.priority = Some(1);
        t1.retry_count = 2;
        // Set last_failure_at far enough in the past so backoff has elapsed
        t1.last_failure_at = Some(Utc::now() - Duration::seconds(600));
        insert(&mut tasks, t1);

        // t2 has never failed
        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(1);
        t2.retry_count = 0;
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &no_active_prs(), &HashMap::new(), 10);
        // t2 (0 retries) should sort before t1 (2 retries)
        assert_eq!(plan.new_work.len(), 2);
        assert_eq!(plan.new_work[0], "t2");
        assert_eq!(plan.new_work[1], "t1");
    }
}
