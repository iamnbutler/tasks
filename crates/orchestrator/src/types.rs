//! Orchestrator domain types.
//!
//! EvaluationContext: what the orchestrator receives.
//! QualityEvaluation: what the orchestrator returns.

use models::merge_queue::MergeQueueEntry;
use models::project::Project;
use models::task::Task;
use serde::{Deserialize, Serialize};

/// Context for evaluating a merge queue entry.
///
/// The orchestrator receives the entry, the associated task, and the project.
/// It fetches PR details (diff, CI status, etc.) from GitHub itself using
/// the `pr_url` on the entry — the remote PR is the source of truth.
pub struct EvaluationContext {
    /// The merge queue entry being evaluated.
    pub entry: MergeQueueEntry,
    /// The task that produced this PR.
    pub task: Task,
    /// The project this task belongs to.
    pub project: Project,
}

/// Verdict from the orchestrator's quality evaluation.
///
/// Spec §7.3: The orchestrator evaluates whether a PR meets quality standards
/// before it can be merged. The caller (server run loop) decides whether to
/// act on the verdict based on the current operating mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityEvaluation {
    /// Whether the orchestrator approves this for merge.
    pub approved: bool,
    /// The orchestrator's reasoning for its decision.
    pub reasoning: String,
    /// Specific feedback for the implementor, if the PR needs work.
    /// Used to re-engage the task's agent session.
    pub feedback: Option<String>,
}
