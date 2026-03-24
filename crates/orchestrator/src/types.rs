//! Orchestrator domain types.
//!
//! EvaluationContext: what the orchestrator receives.
//! QualityEvaluation: what the orchestrator returns.
//! ConflictTriage: conflict resolution decision (spec §7.4).

use chrono::{DateTime, Utc};
use models::merge_queue::{ConflictInfo, ConflictType, MergeQueueEntry, MergeStatus};
use models::project::Project;
use models::task::Task;
use serde::{Deserialize, Serialize};

/// Summary of another PR in the merge queue.
///
/// Used to provide context about other pending work when evaluating a PR,
/// so the orchestrator can consider queue ordering and potential conflicts.
#[derive(Debug, Clone, Serialize)]
pub struct QueuedPrSummary {
    /// The PR URL.
    pub pr_url: String,
    /// The associated task's title.
    pub task_title: String,
    /// Current status in the queue.
    pub status: MergeStatus,
    /// When this PR was queued.
    pub queued_at: DateTime<Utc>,
    /// Queue position (1-indexed) for approved entries awaiting merge.
    pub queue_position: Option<u32>,
}

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
    /// Other PRs currently in the merge queue for the same project.
    /// Includes both pending and approved PRs (excluding the current entry).
    /// This allows the orchestrator to consider queue ordering and detect
    /// potential conflicts or dependencies between PRs.
    pub other_queue_entries: Vec<QueuedPrSummary>,
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

/// Context for conflict triage — spec §7.4.
pub struct ConflictContext {
    /// The merge queue entry with conflict.
    pub entry: MergeQueueEntry,
    /// The conflict details.
    pub conflict_info: ConflictInfo,
    /// The associated task.
    pub task: Task,
    /// The project.
    pub project: Project,
    /// Whether a human is currently present (affects triage in Pause mode).
    pub human_present: bool,
    /// Current operating mode.
    pub mode: OperatingMode,
}

/// Operating mode for conflict triage decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingMode {
    Play,
    Pause,
    Stop,
}

/// Resolution strategy decided by conflict triage — spec §7.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictTriage {
    /// The chosen resolution approach.
    pub resolution: ConflictResolution,
    /// Reasoning for this decision.
    pub reasoning: String,
    /// Feedback to provide to the agent (for ReengageAgent resolution).
    pub agent_feedback: Option<String>,
}

/// How to resolve a conflict — spec §7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// Rebase the branch on top of base (mechanical).
    Rebase,
    /// Re-engage the implementor agent with conflict feedback.
    ReengageAgent,
    /// Surface to human for guidance (Pause mode / complex conflicts).
    SurfaceToHuman,
    /// Auto-resolve trivial conflicts (lock files, generated code).
    AutoResolve,
    /// Retry later (GitHub hasn't computed mergeability yet).
    RetryLater,
}

impl ConflictResolution {
    /// Returns true if this resolution requires human interaction.
    pub fn needs_human(&self) -> bool {
        matches!(self, ConflictResolution::SurfaceToHuman)
    }

    /// Returns true if this resolution can be done automatically.
    pub fn is_automatic(&self) -> bool {
        matches!(
            self,
            ConflictResolution::Rebase | ConflictResolution::AutoResolve
        )
    }
}

/// Default triage logic for conflicts — spec §7.4.
///
/// This function implements the mode-aware conflict resolution strategy:
/// - Play mode: resolve autonomously (mechanical or re-engage agent)
/// - Pause mode: surface non-trivial to human, resolve mechanical directly
pub fn default_triage(conflict_info: &ConflictInfo, mode: OperatingMode, human_present: bool) -> ConflictTriage {
    let conflict_type = conflict_info.conflict_type;

    match conflict_type {
        ConflictType::Unknown => ConflictTriage {
            resolution: ConflictResolution::RetryLater,
            reasoning: "GitHub hasn't computed mergeability yet".to_string(),
            agent_feedback: None,
        },
        ConflictType::NeedsRebase => ConflictTriage {
            resolution: ConflictResolution::Rebase,
            reasoning: "Branch is behind base — mechanical rebase".to_string(),
            agent_feedback: None,
        },
        ConflictType::TrivialMerge => ConflictTriage {
            resolution: ConflictResolution::AutoResolve,
            reasoning: "Conflicts in generated/lock files only — auto-resolve".to_string(),
            agent_feedback: None,
        },
        ConflictType::SourceConflict => {
            match mode {
                OperatingMode::Play => ConflictTriage {
                    resolution: ConflictResolution::ReengageAgent,
                    reasoning: "Source conflict in Play mode — re-engage agent to resolve".to_string(),
                    agent_feedback: Some(format!(
                        "Your branch has merge conflicts with the base branch. Please resolve the conflicts in: {}",
                        conflict_info.conflicting_files.join(", ")
                    )),
                },
                OperatingMode::Pause | OperatingMode::Stop => {
                    if human_present {
                        ConflictTriage {
                            resolution: ConflictResolution::SurfaceToHuman,
                            reasoning: "Source conflict with human present — surface for guidance".to_string(),
                            agent_feedback: None,
                        }
                    } else {
                        ConflictTriage {
                            resolution: ConflictResolution::ReengageAgent,
                            reasoning: "Source conflict without human — re-engage agent".to_string(),
                            agent_feedback: Some(format!(
                                "Your branch has merge conflicts with the base branch. Please resolve the conflicts in: {}",
                                conflict_info.conflicting_files.join(", ")
                            )),
                        }
                    }
                }
            }
        }
        ConflictType::ComplexConflict => {
            match mode {
                OperatingMode::Play => {
                    // In Play mode, try agent first for complex conflicts
                    ConflictTriage {
                        resolution: ConflictResolution::ReengageAgent,
                        reasoning: "Complex conflict in Play mode — attempting agent resolution".to_string(),
                        agent_feedback: Some(format!(
                            "Your branch has extensive merge conflicts across multiple files. \
                             Please carefully review and resolve conflicts in: {}. \
                             Consider rebasing on the latest base branch.",
                            conflict_info.conflicting_files.join(", ")
                        )),
                    }
                }
                OperatingMode::Pause | OperatingMode::Stop => {
                    // In Pause/Stop mode, surface complex conflicts to human
                    ConflictTriage {
                        resolution: ConflictResolution::SurfaceToHuman,
                        reasoning: "Complex conflict — requires human guidance".to_string(),
                        agent_feedback: None,
                    }
                }
            }
        }
    }
}
