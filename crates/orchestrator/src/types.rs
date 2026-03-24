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

/// Summary of another merge queue entry for context during evaluation.
///
/// When evaluating a PR, the orchestrator receives summaries of other PRs
/// in the queue. This helps identify dependencies or ordering issues —
/// e.g., if a PR builds on changes from another PR that hasn't merged yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntrySummary {
    /// GitHub PR URL.
    pub pr_url: String,
    /// PR number (parsed from URL).
    pub pr_number: u64,
    /// Title of the associated task.
    pub task_title: String,
    /// Current status of this queue entry.
    pub status: MergeStatus,
    /// When this entry was added to the queue.
    pub queued_at: DateTime<Utc>,
    /// Position in queue (1 = first to merge). Only set for Pending/Approved entries.
    pub queue_position: Option<u32>,
}

impl QueueEntrySummary {
    /// Create a summary from a merge queue entry and its associated task title.
    pub fn from_entry(entry: &MergeQueueEntry, task_title: &str, queue_position: Option<u32>) -> Self {
        // Parse PR number from URL (e.g., "https://github.com/owner/repo/pull/123")
        let pr_number = entry
            .pr_url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        Self {
            pr_url: entry.pr_url.clone(),
            pr_number,
            task_title: task_title.to_string(),
            status: entry.status,
            queued_at: entry.queued_at,
            queue_position,
        }
    }
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
    /// Summaries of other PRs in the queue (for context).
    /// Sorted by queue position (PRs ahead of current entry come first).
    pub queue_context: Vec<QueueEntrySummary>,
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
