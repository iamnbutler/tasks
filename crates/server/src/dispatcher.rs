//! Dispatch evaluation — spec §12.
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

/// Calculate backoff duration for a given retry count (spec §13.2).
/// Base: 5s, multiplier: 2x, max: 300s. No jitter in the check (jitter is for scheduling).
pub fn backoff_duration(retry_count: u32) -> Duration {
    let base_secs = 5i64;
    let secs = base_secs * 2i64.pow(retry_count.min(6));
    Duration::seconds(secs.min(300))
}

/// Evaluate which tasks should be dispatched (spec §12.6).
///
/// Takes current tasks, per-project session limits, and global max.
/// Returns a DispatchPlan with resume candidates and new work in priority order.
///
/// `pending_answers` is the set of task IDs in Question state that have
/// received a message (human or orchestrator). These are resume candidates.
pub fn evaluate(
    tasks: &HashMap<String, Task>,
    pending_answers: &[String],
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

    // 3. Collect new work candidates: Waiting tasks with backoff elapsed.
    let mut candidates: Vec<&Task> = tasks
        .values()
        .filter(|t| {
            if t.state != TaskState::Waiting {
                return false;
            }
            // Check backoff
            if let Some(failure_at) = t.last_failure_at {
                if failure_at + backoff_duration(t.retry_count) > now {
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

    // 5. Sort candidates by priority rules (spec §12.3).
    candidates.sort_by(|a, b| {
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

        // Recency: newer created_at first (descending).
        b.created_at.cmp(&a.created_at)
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
    use std::collections::HashMap;

    /// Helper to create a minimal task in Waiting state.
    fn make_task(id: &str, project: &str) -> Task {
        Task::new(id.to_string(), TaskSource::Internal, id, project.to_string())
    }

    /// Helper to insert a task into a HashMap.
    fn insert(map: &mut HashMap<String, Task>, task: Task) {
        map.insert(task.id.clone(), task);
    }

    #[test]
    fn empty_tasks_returns_empty_plan() {
        let tasks = HashMap::new();
        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
        assert!(plan.resume.is_empty());
        assert!(plan.new_work.is_empty());
    }

    #[test]
    fn resume_candidates_from_pending_answers() {
        let mut tasks = HashMap::new();
        let mut t = make_task("t1", "proj");
        t.state = TaskState::Question;
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &["t1".to_string()], &HashMap::new(), 10);
        assert_eq!(plan.resume, vec!["t1"]);
    }

    #[test]
    fn question_without_answer_not_resumed() {
        let mut tasks = HashMap::new();
        let mut t = make_task("t1", "proj");
        t.state = TaskState::Question;
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
        assert!(plan.resume.is_empty());
    }

    #[test]
    fn waiting_task_becomes_new_work() {
        let mut tasks = HashMap::new();
        let t = make_task("t1", "proj");
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
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

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
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

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
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

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
        // t1 unblocks t3, so t1 before t2. t3 is Blocked, not in new_work.
        assert_eq!(plan.new_work.len(), 2);
        assert_eq!(plan.new_work[0], "t1");
        assert_eq!(plan.new_work[1], "t2");
    }

    #[test]
    fn priority_sorting_recency() {
        let mut tasks = HashMap::new();

        let now = Utc::now();

        let mut t1 = make_task("t1", "proj");
        t1.priority = Some(1);
        t1.created_at = now - Duration::seconds(100);
        insert(&mut tasks, t1);

        let mut t2 = make_task("t2", "proj");
        t2.priority = Some(1);
        t2.created_at = now;
        insert(&mut tasks, t2);

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
        // t2 is newer, so it sorts first at same priority.
        assert_eq!(plan.new_work, vec!["t2", "t1"]);
    }

    #[test]
    fn global_concurrency_limit() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        let plan = evaluate(&tasks, &[], &HashMap::new(), 1);
        assert_eq!(plan.new_work.len(), 1);
    }

    #[test]
    fn per_project_concurrency_limit() {
        let mut tasks = HashMap::new();
        insert(&mut tasks, make_task("t1", "proj"));
        insert(&mut tasks, make_task("t2", "proj"));

        let mut project_limits = HashMap::new();
        project_limits.insert("proj".to_string(), 1);

        let plan = evaluate(&tasks, &[], &project_limits, 10);
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

        let plan = evaluate(&tasks, &[], &HashMap::new(), 2);
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

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
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

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
        assert!(plan.new_work.is_empty());
    }

    #[test]
    fn backoff_duration_values() {
        // retry 0: 5 * 2^0 = 5s
        assert_eq!(backoff_duration(0), Duration::seconds(5));
        // retry 1: 5 * 2^1 = 10s
        assert_eq!(backoff_duration(1), Duration::seconds(10));
        // retry 2: 5 * 2^2 = 20s
        assert_eq!(backoff_duration(2), Duration::seconds(20));
        // retry 6: 5 * 2^6 = 320 → capped at 300s
        assert_eq!(backoff_duration(6), Duration::seconds(300));
        // retry 10: capped at retry_count.min(6), so 5 * 2^6 = 320 → capped at 300s
        assert_eq!(backoff_duration(10), Duration::seconds(300));
    }

    #[test]
    fn elapsed_backoff_allows_dispatch() {
        let mut tasks = HashMap::new();

        let mut t = make_task("t1", "proj");
        t.retry_count = 1;
        // Failed 20 seconds ago; backoff for retry_count=1 is 10s, so it should be allowed.
        t.last_failure_at = Some(Utc::now() - Duration::seconds(20));
        insert(&mut tasks, t);

        let plan = evaluate(&tasks, &[], &HashMap::new(), 10);
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

        let plan = evaluate(&tasks, &[], &project_limits, 10);
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
            &HashMap::new(),
            2,
        );
        assert_eq!(plan.resume, vec!["q1"]);
        assert_eq!(plan.new_work, vec!["t1"]);
    }
}
