//! Orchestrator trait — the core abstraction.
//!
//! Spec §4.2: The orchestrator evaluates work quality and provides feedback.
//! Spec §7.4: The orchestrator triages conflicts with mode-aware resolution.
//!
//! The trait is intentionally narrow — `evaluate` returns a verdict, and the
//! caller (server run loop) decides whether to act on it based on mode.

use crate::error::OrchestratorError;
use crate::types::{ConflictContext, ConflictTriage, EvaluationContext, QualityEvaluation};
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
}
