//! Orchestrator trait — the core abstraction.
//!
//! Spec §4.2: The orchestrator is an AI agent that manages the project.
//! It evaluates work quality, triages conflicts, and proactively manages
//! project state through periodic reasoning passes.
//!
//! The orchestrator is an actor: it periodically surveys system state,
//! identifies patterns, and returns actions. The event bus is a data
//! source it can consult, not its driver.

use crate::error::OrchestratorError;
use crate::types::{
    ConflictContext, ConflictTriage, EvaluationContext, OrchestratorAction, QualityEvaluation,
    QuestionContext, SystemContext, TriageContext, TriageResult,
};
use models::task::Task;

/// Trait for orchestrator implementations.
///
/// The orchestrator is pluggable — `ClaudeOrchestrator` is the default
/// implementation, but tests can use a `MockOrchestrator`.
#[trait_variant::make(Send)]
pub trait Orchestrator: Sync {
    /// Evaluate a merge queue entry for merge worthiness.
    ///
    /// Spec §7.3: Checks issue alignment, test/CI status, conflicts,
    /// and project conventions. Returns a verdict with reasoning.
    ///
    /// The orchestrator fetches PR details from GitHub using the
    /// `pr_url` on the entry — it is not given stale inline data.
    async fn evaluate(
        &self,
        context: &EvaluationContext,
    ) -> Result<QualityEvaluation, OrchestratorError>;

    /// Send feedback to a task's agent session.
    ///
    /// Used after rejection to re-engage the implementor with specific
    /// guidance on what needs to change.
    async fn feedback(
        &self,
        task: &Task,
        feedback: &str,
    ) -> Result<(), OrchestratorError>;

    /// Triage a merge conflict and decide resolution strategy.
    ///
    /// Spec §7.4: The orchestrator triages conflicts based on type and mode:
    /// - Play mode: resolve autonomously (rebase, re-engage agent)
    /// - Pause mode: surface non-trivial to human, resolve mechanical directly
    ///
    /// The default implementation uses `default_triage()` from types.rs.
    /// Implementations can override for custom triage logic.
    async fn triage_conflict(
        &self,
        context: &ConflictContext,
    ) -> Result<ConflictTriage, OrchestratorError>;

    /// Periodic reasoning pass — the orchestrator surveys system state and
    /// decides what actions to take.
    ///
    /// Called on a regular interval (~30s) by the run loop. The orchestrator
    /// receives a full snapshot of system state and recent events, identifies
    /// patterns, and returns actions (narration, state changes, priorities).
    ///
    /// This is NOT reactive to individual events — the orchestrator stands
    /// outside the event stream and sees the full picture.
    async fn think(
        &self,
        context: &SystemContext,
    ) -> Result<Vec<OrchestratorAction>, OrchestratorError>;

    /// Answer a stuck agent's question.
    ///
    /// When an agent session enters the Question state, the orchestrator
    /// generates actionable guidance based on the task context and the
    /// agent's question. The answer is sent back to the agent session
    /// as a chat message.
    async fn answer_question(
        &self,
        context: &QuestionContext,
    ) -> Result<String, OrchestratorError>;

    /// Triage and decompose natural language into actionable issues.
    ///
    /// Spec §4.2: When the human describes work in natural language, the
    /// orchestrator turns it into well-formed issues — organized, contextualized,
    /// and split into sub-issues as needed.
    ///
    /// Returns a `TriageResult` with draft issues for human review. The caller
    /// is responsible for presenting these to the human and creating them on
    /// GitHub after approval.
    async fn triage(
        &self,
        context: &TriageContext,
    ) -> Result<TriageResult, OrchestratorError>;
}
