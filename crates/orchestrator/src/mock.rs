//! Mock orchestrator for testing.

use std::sync::Mutex;

use crate::error::OrchestratorError;
use crate::orchestrator::Orchestrator;
use crate::types::{
    default_triage, ConflictContext, ConflictTriage, EvaluationContext, IssueDraft,
    OrchestratorAction, QualityEvaluation, QuestionContext, SystemContext, TriageContext,
    TriageResult,
};
use models::task::Task;

/// A mock orchestrator that returns configurable responses.
///
/// Used in tests to verify server integration without hitting
/// a real LLM.
pub struct MockOrchestrator {
    /// The evaluation to return from `evaluate()`.
    evaluation: Mutex<QualityEvaluation>,
    /// Optional conflict triage override (uses default_triage if None).
    conflict_triage: Mutex<Option<ConflictTriage>>,
    /// Count of evaluate calls (for assertions).
    pub evaluate_count: Mutex<u32>,
    /// Count of feedback calls (for assertions).
    pub feedback_count: Mutex<u32>,
    /// Count of triage_conflict calls (for assertions).
    pub triage_count: Mutex<u32>,
    /// Count of think calls (for assertions).
    pub think_count: Mutex<u32>,
    /// Count of answer_question calls (for assertions).
    pub answer_question_count: Mutex<u32>,
    /// Count of triage calls (for assertions).
    pub triage_decompose_count: Mutex<u32>,
    /// Optional triage result override.
    triage_result: Mutex<Option<TriageResult>>,
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
            conflict_triage: Mutex::new(None),
            evaluate_count: Mutex::new(0),
            feedback_count: Mutex::new(0),
            triage_count: Mutex::new(0),
            think_count: Mutex::new(0),
            answer_question_count: Mutex::new(0),
            triage_decompose_count: Mutex::new(0),
            triage_result: Mutex::new(None),
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
            conflict_triage: Mutex::new(None),
            evaluate_count: Mutex::new(0),
            feedback_count: Mutex::new(0),
            triage_count: Mutex::new(0),
            think_count: Mutex::new(0),
            answer_question_count: Mutex::new(0),
            triage_decompose_count: Mutex::new(0),
            triage_result: Mutex::new(None),
        }
    }

    /// Set what evaluation to return next.
    pub fn set_evaluation(&self, eval: QualityEvaluation) {
        *self.evaluation.lock().unwrap() = eval;
    }

    /// Set what conflict triage to return next.
    pub fn set_conflict_triage(&self, triage: ConflictTriage) {
        *self.conflict_triage.lock().unwrap() = Some(triage);
    }

    /// Set what triage decomposition to return next.
    pub fn set_triage_result(&self, result: TriageResult) {
        *self.triage_result.lock().unwrap() = Some(result);
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
        context: &ConflictContext,
    ) -> Result<ConflictTriage, OrchestratorError> {
        *self.triage_count.lock().unwrap() += 1;
        // Return configured triage if set, otherwise use default logic
        let override_triage = self.conflict_triage.lock().unwrap().clone();
        Ok(override_triage.unwrap_or_else(|| {
            default_triage(&context.conflict_info, context.mode, context.human_present)
        }))
    }

    async fn think(
        &self,
        _context: &SystemContext,
    ) -> Result<Vec<OrchestratorAction>, OrchestratorError> {
        *self.think_count.lock().unwrap() += 1;
        // Mock returns no actions — tests can verify call count.
        Ok(Vec::new())
    }

    async fn answer_question(
        &self,
        _context: &QuestionContext,
    ) -> Result<String, OrchestratorError> {
        *self.answer_question_count.lock().unwrap() += 1;
        Ok("Mock: proceed with whatever approach you think is best.".to_string())
    }

    async fn triage(
        &self,
        _context: &TriageContext,
    ) -> Result<TriageResult, OrchestratorError> {
        *self.triage_decompose_count.lock().unwrap() += 1;
        let override_result = self.triage_result.lock().unwrap().clone();
        Ok(override_result.unwrap_or_else(|| TriageResult {
            analysis: "Mock: single issue created from request.".to_string(),
            issues: vec![IssueDraft {
                title: "Mock issue".to_string(),
                body: "Created by mock orchestrator.".to_string(),
                labels: vec![],
                blocked_by: vec![],
            }],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConflictResolution, EvaluationContext, OperatingMode};
    use models::merge_queue::{ConflictInfo, ConflictType, MergeQueueEntry};
    use models::project::Project;
    use models::task::{Task, TaskSource};

    fn test_context() -> EvaluationContext {
        EvaluationContext {
            entry: MergeQueueEntry::new("mq-1", "task-1", "https://github.com/owner/repo/pull/1"),
            task: Task::new("task-1", TaskSource::Internal, "Test task", "proj-1"),
            project: Project::new("proj-1", "owner/repo"),
            queue_context: Vec::new(),
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

    fn test_conflict_context(conflict_type: ConflictType, mode: OperatingMode) -> ConflictContext {
        ConflictContext {
            entry: MergeQueueEntry::new("mq-1", "task-1", "https://github.com/owner/repo/pull/1"),
            conflict_info: ConflictInfo::new(conflict_type, "test conflict"),
            task: Task::new("task-1", TaskSource::Internal, "Test task", "proj-1"),
            project: Project::new("proj-1", "owner/repo"),
            human_present: false,
            mode,
        }
    }

    #[tokio::test]
    async fn test_triage_needs_rebase() {
        let mock = MockOrchestrator::approving();
        let ctx = test_conflict_context(ConflictType::NeedsRebase, OperatingMode::Play);
        let result = mock.triage_conflict(&ctx).await.unwrap();
        assert_eq!(result.resolution, ConflictResolution::Rebase);
        assert_eq!(*mock.triage_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_triage_trivial_auto_resolves() {
        let mock = MockOrchestrator::approving();
        let ctx = test_conflict_context(ConflictType::TrivialMerge, OperatingMode::Play);
        let result = mock.triage_conflict(&ctx).await.unwrap();
        assert_eq!(result.resolution, ConflictResolution::AutoResolve);
    }

    #[tokio::test]
    async fn test_triage_source_conflict_play_mode_reengages() {
        let mock = MockOrchestrator::approving();
        let ctx = test_conflict_context(ConflictType::SourceConflict, OperatingMode::Play);
        let result = mock.triage_conflict(&ctx).await.unwrap();
        assert_eq!(result.resolution, ConflictResolution::ReengageAgent);
        assert!(result.agent_feedback.is_some());
    }

    #[tokio::test]
    async fn test_triage_complex_conflict_pause_mode_surfaces() {
        let mock = MockOrchestrator::approving();
        let ctx = test_conflict_context(ConflictType::ComplexConflict, OperatingMode::Pause);
        let result = mock.triage_conflict(&ctx).await.unwrap();
        assert_eq!(result.resolution, ConflictResolution::SurfaceToHuman);
    }

    #[tokio::test]
    async fn test_triage_unknown_retries_later() {
        let mock = MockOrchestrator::approving();
        let ctx = test_conflict_context(ConflictType::Unknown, OperatingMode::Play);
        let result = mock.triage_conflict(&ctx).await.unwrap();
        assert_eq!(result.resolution, ConflictResolution::RetryLater);
    }

    #[tokio::test]
    async fn test_triage_can_be_overridden() {
        let mock = MockOrchestrator::approving();
        mock.set_conflict_triage(ConflictTriage {
            resolution: ConflictResolution::SurfaceToHuman,
            reasoning: "custom triage".to_string(),
            agent_feedback: None,
        });
        let ctx = test_conflict_context(ConflictType::NeedsRebase, OperatingMode::Play);
        let result = mock.triage_conflict(&ctx).await.unwrap();
        // Should use the override, not the default (which would be Rebase)
        assert_eq!(result.resolution, ConflictResolution::SurfaceToHuman);
        assert_eq!(result.reasoning, "custom triage");
    }

    #[tokio::test]
    async fn test_answer_question() {
        let mock = MockOrchestrator::approving();
        let ctx = QuestionContext {
            task: Task::new("task-1", TaskSource::Internal, "Test task", "proj-1"),
            project: Project::new("proj-1", "owner/repo"),
            question: "How should I structure the database schema?".to_string(),
            human_present: false,
        };
        let result = mock.answer_question(&ctx).await.unwrap();
        assert!(!result.is_empty());
        assert_eq!(*mock.answer_question_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_triage_default() {
        let mock = MockOrchestrator::approving();
        let ctx = TriageContext {
            description: "Add rate limiting to the API".to_string(),
            project: Project::new("proj-1", "owner/repo"),
            existing_issues: vec![],
        };
        let result = mock.triage(&ctx).await.unwrap();
        assert!(!result.issues.is_empty());
        assert_eq!(*mock.triage_decompose_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_triage_custom_result() {
        let mock = MockOrchestrator::approving();
        mock.set_triage_result(TriageResult {
            analysis: "Custom analysis".to_string(),
            issues: vec![
                IssueDraft {
                    title: "First issue".to_string(),
                    body: "Body 1".to_string(),
                    labels: vec!["enhancement".to_string()],
                    blocked_by: vec![],
                },
                IssueDraft {
                    title: "Second issue".to_string(),
                    body: "Body 2".to_string(),
                    labels: vec![],
                    blocked_by: vec![0],
                },
            ],
        });
        let ctx = TriageContext {
            description: "Something".to_string(),
            project: Project::new("proj-1", "owner/repo"),
            existing_issues: vec![],
        };
        let result = mock.triage(&ctx).await.unwrap();
        assert_eq!(result.analysis, "Custom analysis");
        assert_eq!(result.issues.len(), 2);
        assert_eq!(result.issues[1].blocked_by, vec![0]);
    }
}
