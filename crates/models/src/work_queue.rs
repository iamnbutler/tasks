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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- WorkType::as_str / from_str round-trip ----

    #[test]
    fn work_type_as_str_values() {
        assert_eq!(WorkType::MergeConflict.as_str(), "merge_conflict");
        assert_eq!(WorkType::PrFeedback.as_str(), "pr_feedback");
        assert_eq!(WorkType::Automation.as_str(), "automation");
        assert_eq!(WorkType::Task.as_str(), "task");
    }

    #[test]
    fn work_type_from_str_valid() {
        assert_eq!(
            WorkType::from_str("merge_conflict"),
            Some(WorkType::MergeConflict)
        );
        assert_eq!(
            WorkType::from_str("pr_feedback"),
            Some(WorkType::PrFeedback)
        );
        assert_eq!(
            WorkType::from_str("automation"),
            Some(WorkType::Automation)
        );
        assert_eq!(WorkType::from_str("task"), Some(WorkType::Task));
    }

    #[test]
    fn work_type_from_str_invalid() {
        assert_eq!(WorkType::from_str("invalid"), None);
        assert_eq!(WorkType::from_str(""), None);
        assert_eq!(WorkType::from_str("TASK"), None); // case sensitive
        assert_eq!(WorkType::from_str("merge-conflict"), None); // wrong separator
    }

    #[test]
    fn work_type_round_trip() {
        let all_types = [
            WorkType::MergeConflict,
            WorkType::PrFeedback,
            WorkType::Automation,
            WorkType::Task,
        ];
        for work_type in all_types {
            let s = work_type.as_str();
            let recovered = WorkType::from_str(s);
            assert_eq!(
                recovered,
                Some(work_type),
                "Round-trip failed for {work_type:?}"
            );
        }
    }

    // ---- WorkType ordering (priority) ----

    #[test]
    fn work_type_ordering_merge_conflict_highest_priority() {
        // MergeConflict should sort before all others (lowest enum value = highest priority)
        assert!(WorkType::MergeConflict < WorkType::PrFeedback);
        assert!(WorkType::MergeConflict < WorkType::Automation);
        assert!(WorkType::MergeConflict < WorkType::Task);
    }

    #[test]
    fn work_type_ordering_pr_feedback_second() {
        assert!(WorkType::PrFeedback > WorkType::MergeConflict);
        assert!(WorkType::PrFeedback < WorkType::Automation);
        assert!(WorkType::PrFeedback < WorkType::Task);
    }

    #[test]
    fn work_type_ordering_automation_third() {
        assert!(WorkType::Automation > WorkType::MergeConflict);
        assert!(WorkType::Automation > WorkType::PrFeedback);
        assert!(WorkType::Automation < WorkType::Task);
    }

    #[test]
    fn work_type_ordering_task_lowest_priority() {
        assert!(WorkType::Task > WorkType::MergeConflict);
        assert!(WorkType::Task > WorkType::PrFeedback);
        assert!(WorkType::Task > WorkType::Automation);
    }

    #[test]
    fn work_type_ordering_full_sequence() {
        // Verify the complete priority order: MergeConflict < PrFeedback < Automation < Task
        let mut types = vec![
            WorkType::Task,
            WorkType::MergeConflict,
            WorkType::Automation,
            WorkType::PrFeedback,
        ];
        types.sort();
        assert_eq!(
            types,
            vec![
                WorkType::MergeConflict,
                WorkType::PrFeedback,
                WorkType::Automation,
                WorkType::Task,
            ]
        );
    }

    // ---- WorkItem::new ----

    #[test]
    fn work_item_new_id_format() {
        let item = WorkItem::new(WorkType::Task, "task-123", "project-1");
        assert_eq!(item.id, "task:task-123");

        let item = WorkItem::new(WorkType::MergeConflict, "pr-456", "project-2");
        assert_eq!(item.id, "merge_conflict:pr-456");

        let item = WorkItem::new(WorkType::PrFeedback, "pr-789", "project-3");
        assert_eq!(item.id, "pr_feedback:pr-789");

        let item = WorkItem::new(WorkType::Automation, "run-001", "project-4");
        assert_eq!(item.id, "automation:run-001");
    }

    #[test]
    fn work_item_new_fields() {
        let item = WorkItem::new(WorkType::Task, "task-123", "project-1");

        assert_eq!(item.work_type, WorkType::Task);
        assert_eq!(item.source_id, "task-123");
        assert_eq!(item.project_id, "project-1");
        assert_eq!(item.priority, 0); // default priority
        assert!(item.claimed_at.is_none());
        assert!(item.claimed_by.is_none());
    }

    #[test]
    fn work_item_new_accepts_string_types() {
        // Test that impl Into<String> works with various types
        let item1 = WorkItem::new(WorkType::Task, "str_slice", "project");
        assert_eq!(item1.source_id, "str_slice");

        let item2 = WorkItem::new(WorkType::Task, String::from("owned_string"), "project");
        assert_eq!(item2.source_id, "owned_string");
    }

    // ---- WorkItem::is_claimed ----

    #[test]
    fn work_item_is_claimed_false_when_unclaimed() {
        let item = WorkItem::new(WorkType::Task, "task-123", "project-1");
        assert!(!item.is_claimed());
    }

    #[test]
    fn work_item_is_claimed_true_when_claimed_by_set() {
        let mut item = WorkItem::new(WorkType::Task, "task-123", "project-1");
        item.claimed_by = Some("container-abc".to_string());
        assert!(item.is_claimed());
    }

    #[test]
    fn work_item_is_claimed_checks_claimed_by_not_claimed_at() {
        // is_claimed() checks claimed_by, not claimed_at
        let mut item = WorkItem::new(WorkType::Task, "task-123", "project-1");

        // Only claimed_at set, not claimed_by
        item.claimed_at = Some(Utc::now());
        item.claimed_by = None;
        assert!(!item.is_claimed());

        // Only claimed_by set, not claimed_at
        item.claimed_at = None;
        item.claimed_by = Some("container-abc".to_string());
        assert!(item.is_claimed());
    }

    // ---- Builder methods ----

    #[test]
    fn work_item_with_priority() {
        let item = WorkItem::new(WorkType::Task, "task-123", "project-1").with_priority(10);
        assert_eq!(item.priority, 10);
    }

    #[test]
    fn work_item_with_priority_chaining() {
        let item = WorkItem::new(WorkType::Task, "task-123", "project-1")
            .with_priority(5)
            .with_priority(15); // second call overwrites
        assert_eq!(item.priority, 15);
    }

    #[test]
    fn work_item_with_created_at() {
        let custom_time = DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let item =
            WorkItem::new(WorkType::Task, "task-123", "project-1").with_created_at(custom_time);
        assert_eq!(item.created_at, custom_time);
    }

    #[test]
    fn work_item_builder_chaining() {
        let custom_time = DateTime::parse_from_rfc3339("2024-06-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let item = WorkItem::new(WorkType::Automation, "run-42", "my-project")
            .with_priority(100)
            .with_created_at(custom_time);

        assert_eq!(item.id, "automation:run-42");
        assert_eq!(item.work_type, WorkType::Automation);
        assert_eq!(item.source_id, "run-42");
        assert_eq!(item.project_id, "my-project");
        assert_eq!(item.priority, 100);
        assert_eq!(item.created_at, custom_time);
        assert!(!item.is_claimed());
    }

    // ---- Serde serialization ----

    #[test]
    fn work_type_serde_round_trip() {
        let all_types = [
            WorkType::MergeConflict,
            WorkType::PrFeedback,
            WorkType::Automation,
            WorkType::Task,
        ];

        for work_type in all_types {
            let json = serde_json::to_string(&work_type).unwrap();
            let recovered: WorkType = serde_json::from_str(&json).unwrap();
            assert_eq!(
                recovered, work_type,
                "Serde round-trip failed for {work_type:?}"
            );
        }
    }

    #[test]
    fn work_type_serde_snake_case() {
        // Verify serde uses snake_case as configured
        assert_eq!(
            serde_json::to_string(&WorkType::MergeConflict).unwrap(),
            "\"merge_conflict\""
        );
        assert_eq!(
            serde_json::to_string(&WorkType::PrFeedback).unwrap(),
            "\"pr_feedback\""
        );
        assert_eq!(
            serde_json::to_string(&WorkType::Automation).unwrap(),
            "\"automation\""
        );
        assert_eq!(
            serde_json::to_string(&WorkType::Task).unwrap(),
            "\"task\""
        );
    }

    #[test]
    fn work_item_serde_round_trip() {
        let item = WorkItem::new(WorkType::PrFeedback, "pr-999", "test-project")
            .with_priority(50);

        let json = serde_json::to_string(&item).unwrap();
        let recovered: WorkItem = serde_json::from_str(&json).unwrap();

        assert_eq!(recovered.id, item.id);
        assert_eq!(recovered.work_type, item.work_type);
        assert_eq!(recovered.source_id, item.source_id);
        assert_eq!(recovered.project_id, item.project_id);
        assert_eq!(recovered.priority, item.priority);
    }
}
