//! Orchestrator trait — the core abstraction.
//!
//! Spec §4.2: The orchestrator evaluates work quality and provides feedback.
//! Spec §7.4: The orchestrator triages conflicts with mode-aware behavior.
//!
//! The trait is intentionally narrow — `evaluate` returns a verdict, and the
//! caller (server run loop) decides whether to act on it based on mode.

use crate::error::OrchestratorError;
use crate::types::{ConflictTriage, EvaluationContext, QualityEvaluation};
use models::merge_queue::ConflictInfo;
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

    /// Triage a detected conflict and decide how to resolve it (spec §7.4).
    ///
    /// The orchestrator examines the conflict details and current context
    /// to decide the appropriate resolution strategy:
    /// - Mechanical conflicts (rebase, trivial merge) are resolved directly
    /// - Source conflicts may re-engage the implementor agent
    /// - Complex conflicts may be surfaced to the human
    ///
    /// The `is_play_mode` and `human_present` flags inform the triage decision
    /// per spec §7.4's mode-aware behavior.
    async fn triage_conflict(
        &self,
        entry_id: &str,
        conflict_info: &ConflictInfo,
        is_play_mode: bool,
        human_present: bool,
    ) -> Result<ConflictTriage, OrchestratorError>;
}
