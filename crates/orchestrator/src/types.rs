//! Orchestrator domain types.
//!
//! EvaluationContext: what the orchestrator receives.
//! QualityEvaluation: what the orchestrator returns.
//! ConflictTriage: conflict resolution decisions (spec §7.4).

use models::merge_queue::{ConflictInfo, ConflictType, MergeQueueEntry};
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

/// How the orchestrator decides to resolve a conflict (spec §7.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Rebase the branch onto the base branch.
    /// Mechanical resolution for branches that are simply behind.
    Rebase,
    /// Re-engage the implementor agent to resolve the conflict.
    /// Used for source conflicts that an agent can likely handle.
    ReengageAgent {
        /// Specific instructions for the agent about the conflict.
        instructions: String,
    },
    /// Surface to human for guidance.
    /// Used for complex conflicts in Pause mode or when human is present.
    SurfaceToHuman {
        /// Description of what needs human attention.
        summary: String,
    },
    /// Attempt automatic merge conflict resolution.
    /// Used for trivial conflicts (lockfiles, generated files).
    AutoResolve {
        /// Strategy to use (e.g., "ours", "theirs", "regenerate").
        strategy: String,
    },
    /// Wait and retry later (GitHub hasn't computed mergeability yet).
    RetryLater,
}

/// Result of conflict triage — the orchestrator's decision on how to handle
/// a detected conflict (spec §7.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictTriage {
    /// The entry ID being triaged.
    pub entry_id: String,
    /// The detected conflict information.
    pub conflict_info: ConflictInfo,
    /// How to resolve this conflict.
    pub resolution: ConflictResolution,
    /// The orchestrator's reasoning for this decision.
    pub reasoning: String,
}

impl ConflictTriage {
    /// Create a new conflict triage decision.
    pub fn new(
        entry_id: impl Into<String>,
        conflict_info: ConflictInfo,
        resolution: ConflictResolution,
        reasoning: impl Into<String>,
    ) -> Self {
        Self {
            entry_id: entry_id.into(),
            conflict_info,
            resolution,
            reasoning: reasoning.into(),
        }
    }

    /// Create a triage decision for a mechanical rebase.
    pub fn rebase(entry_id: impl Into<String>, conflict_info: ConflictInfo) -> Self {
        Self::new(
            entry_id,
            conflict_info,
            ConflictResolution::Rebase,
            "Branch is behind base, can be resolved with rebase",
        )
    }

    /// Create a triage decision to re-engage the agent.
    pub fn reengage_agent(
        entry_id: impl Into<String>,
        conflict_info: ConflictInfo,
        instructions: impl Into<String>,
    ) -> Self {
        Self::new(
            entry_id,
            conflict_info,
            ConflictResolution::ReengageAgent {
                instructions: instructions.into(),
            },
            "Source conflict detected, re-engaging implementor agent",
        )
    }

    /// Create a triage decision to surface to human.
    pub fn surface_to_human(
        entry_id: impl Into<String>,
        conflict_info: ConflictInfo,
        summary: impl Into<String>,
    ) -> Self {
        Self::new(
            entry_id,
            conflict_info,
            ConflictResolution::SurfaceToHuman {
                summary: summary.into(),
            },
            "Complex conflict requires human guidance",
        )
    }

    /// Create a triage decision to retry later.
    pub fn retry_later(entry_id: impl Into<String>, conflict_info: ConflictInfo) -> Self {
        Self::new(
            entry_id,
            conflict_info,
            ConflictResolution::RetryLater,
            "GitHub has not yet computed mergeability, will retry",
        )
    }
}

/// Default triage logic based on conflict type and mode.
///
/// This implements the spec §7.4 triage rules:
/// - Play mode: resolve autonomously (rebase/agent)
/// - Pause mode: surface non-trivial to human, resolve mechanical directly
pub fn default_triage(
    entry_id: &str,
    conflict_info: &ConflictInfo,
    is_play_mode: bool,
    human_present: bool,
) -> ConflictTriage {
    let entry_id = entry_id.to_string();
    let info = conflict_info.clone();

    match conflict_info.conflict_type {
        ConflictType::NeedsRebase => {
            // Mechanical — always resolve with rebase
            ConflictTriage::rebase(entry_id, info)
        }
        ConflictType::TrivialMerge => {
            // Mechanical — auto-resolve
            ConflictTriage::new(
                entry_id,
                info,
                ConflictResolution::AutoResolve {
                    strategy: "regenerate".to_string(),
                },
                "Trivial conflict in generated files, auto-resolving",
            )
        }
        ConflictType::SourceConflict => {
            if is_play_mode {
                // Play mode: re-engage agent
                let files = conflict_info.conflicting_files.join(", ");
                let instructions = format!(
                    "Merge conflict detected in: {}. Please resolve the conflicts and update the PR.",
                    if files.is_empty() { "unknown files" } else { &files }
                );
                ConflictTriage::reengage_agent(entry_id, info, instructions)
            } else if human_present {
                // Pause mode with human: surface to human
                ConflictTriage::surface_to_human(
                    entry_id,
                    info,
                    format!(
                        "Source conflict in {} file(s). Agent can be re-engaged, or resolve manually.",
                        conflict_info.conflicting_files.len()
                    ),
                )
            } else {
                // Pause mode without human: re-engage agent for source conflicts
                let files = conflict_info.conflicting_files.join(", ");
                ConflictTriage::reengage_agent(
                    entry_id,
                    info,
                    format!("Resolve merge conflicts in: {}", files),
                )
            }
        }
        ConflictType::ComplexConflict => {
            if human_present || !is_play_mode {
                // Surface to human
                ConflictTriage::surface_to_human(
                    entry_id,
                    info,
                    "Complex merge conflict requires human review and guidance",
                )
            } else {
                // Play mode without human: still try to re-engage agent
                // but flag that this may need escalation
                ConflictTriage::reengage_agent(
                    entry_id,
                    info,
                    "Complex merge conflict detected. Please attempt to resolve, or ask for help if needed.",
                )
            }
        }
        ConflictType::Unknown => {
            // GitHub hasn't computed yet — retry later
            ConflictTriage::retry_later(entry_id, info)
        }
    }
}
