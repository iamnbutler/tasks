//! Mock orchestrator for testing.

use std::sync::Mutex;

use crate::error::OrchestratorError;
use crate::orchestrator::Orchestrator;
use crate::types::{default_triage, ConflictTriage, EvaluationContext, QualityEvaluation};
use models::merge_queue::ConflictInfo;
use models::task::Task;

/// A mock orchestrator that returns configurable responses.
///
/// Used in tests to verify server integration without hitting
/// a real LLM.
pub struct MockOrchestrator {
    /// The evaluation to return from `evaluate()`.
    evaluation: Mutex<QualityEvaluation>,
    /// Count of evaluate calls (for assertions).
    pub evaluate_count: Mutex<u32>,
    /// Count of feedback calls (for assertions).
    pub feedback_count: Mutex<u32>,
    /// Count of triage calls (for assertions).
    pub triage_count: Mutex<u32>,
}

impl MockOrchestrator {
    /// Create a mock that always approves.
    pub fn approving() -> Self {
        Self {
            evaluation: Mutex::new(QualityEvaluation {
                approved: true,
                reasoning: "Mock: approved".to_string(),
                feedback: None,
            }),
            evaluate_count: Mutex::new(0),
            feedback_count: Mutex::new(0),
            triage_count: Mutex::new(0),
        }
    }

    /// Create a mock that always rejects with the given feedback.
    pub fn rejecting(feedback: impl Into<String>) -> Self {
        let feedback = feedback.into();
        Self {
            evaluation: Mutex::new(QualityEvaluation {
                approved: false,
                reasoning: "Mock: rejected".to_string(),
                feedback: Some(feedback),
            }),
            evaluate_count: Mutex::new(0),
            feedback_count: Mutex::new(0),
            triage_count: Mutex::new(0),
        }
    }

    /// Set what evaluation to return next.
    pub fn set_evaluation(&self, eval: QualityEvaluation) {
        *self.evaluation.lock().unwrap() = eval;
    }
}

impl Orchestrator for MockOrchestrator {
    async fn evaluate(
        &self,
        _context: &EvaluationContext,
    ) -> Result<QualityEvaluation, OrchestratorError> {
        *self.evaluate_count.lock().unwrap() += 1;
        Ok(self.evaluation.lock().unwrap().clone())
    }

    async fn feedback(
        &self,
        _task: &Task,
        _feedback: &str,
    ) -> Result<(), OrchestratorError> {
        *self.feedback_count.lock().unwrap() += 1;
        Ok(())
    }

    async fn triage_conflict(
        &self,
        entry_id: &str,
        conflict_info: &ConflictInfo,
        is_play_mode: bool,
        human_present: bool,
    ) -> Result<ConflictTriage, OrchestratorError> {
        *self.triage_count.lock().unwrap() += 1;
        // Use default triage logic for mock
        Ok(default_triage(entry_id, conflict_info, is_play_mode, human_present))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EvaluationContext;
    use models::merge_queue::MergeQueueEntry;
    use models::project::Project;
    use models::task::{Task, TaskSource};

    fn test_context() -> EvaluationContext {
        EvaluationContext {
            entry: MergeQueueEntry::new("mq-1", "task-1", "https://github.com/owner/repo/pull/1"),
            task: Task::new("task-1", TaskSource::Internal, "Test task", "proj-1"),
            project: Project::new("proj-1", "owner/repo"),
        }
    }

    #[tokio::test]
    async fn test_mock_approving() {
        let mock = MockOrchestrator::approving();
        let ctx = test_context();
        let result = mock.evaluate(&ctx).await.unwrap();
        assert!(result.approved);
        assert_eq!(*mock.evaluate_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_mock_rejecting() {
        let mock = MockOrchestrator::rejecting("needs tests");
        let ctx = test_context();
        let result = mock.evaluate(&ctx).await.unwrap();
        assert!(!result.approved);
        assert_eq!(result.feedback, Some("needs tests".to_string()));
    }

    #[tokio::test]
    async fn test_mock_feedback() {
        let mock = MockOrchestrator::approving();
        let task = Task::new("task-1", TaskSource::Internal, "Test task", "proj-1");
        mock.feedback(&task, "fix the tests").await.unwrap();
        assert_eq!(*mock.feedback_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_mock_set_evaluation() {
        let mock = MockOrchestrator::approving();
        mock.set_evaluation(QualityEvaluation {
            approved: false,
            reasoning: "changed my mind".to_string(),
            feedback: Some("redo everything".to_string()),
        });
        let ctx = test_context();
        let result = mock.evaluate(&ctx).await.unwrap();
        assert!(!result.approved);
        assert_eq!(result.reasoning, "changed my mind");
    }
}
