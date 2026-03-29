//! Orchestrator domain types.
//!
//! EvaluationContext: what the orchestrator receives.
//! QualityEvaluation: what the orchestrator returns.
//! ConflictTriage: conflict resolution decision (spec §7.4).
//! SystemContext: snapshot of current system state for event processing.
//! OrchestratorAction: actions the orchestrator can request.

use chrono::{DateTime, Utc};
use models::merge_queue::{ConflictInfo, ConflictType, MergeQueueEntry, MergeStatus};
use models::project::Project;
use models::task::{Task, TaskState};
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
    /// Context that was unavailable during evaluation (e.g. "pr_diff", "linked_issue").
    /// Empty means the evaluation had full context.
    #[serde(default)]
    pub missing_context: Vec<String>,
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

/// Context for answering a stuck agent's question.
///
/// When an agent enters Question state, the orchestrator receives this context
/// and generates actionable guidance to unblock the agent.
pub struct QuestionContext {
    /// The task the agent is working on.
    pub task: Task,
    /// The project this task belongs to.
    pub project: Project,
    /// The agent's question text (extracted from the most recent agent:question event).
    pub question: String,
    /// Whether a human is currently present (GUI connected).
    ///
    /// Note: The decision to answer vs escalate is made by the caller (run_loop)
    /// before calling `answer_question()`. This field is passed for context and
    /// potential future use in prompt customization, but is not currently read
    /// by the answer generation logic.
    pub human_present: bool,
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

/// Snapshot of current system state for the orchestrator's think() pass.
///
/// The orchestrator receives the full picture — tasks, merge queue, recent
/// events — and can identify patterns across them. This is NOT per-event
/// context; it's a periodic survey of the entire landscape.
#[derive(Debug, Clone)]
pub struct SystemContext {
    /// Current operating mode.
    pub mode: OperatingMode,
    /// All projects.
    pub projects: Vec<Project>,
    /// All tasks with their current state.
    pub tasks: Vec<Task>,
    /// Merge queue entries.
    pub merge_queue: Vec<MergeQueueEntry>,
    /// Whether a human is currently connected.
    pub human_present: bool,
    /// Recent events since the last think() call (for pattern detection).
    pub recent_events: Vec<events::Event>,
    /// When the last think() pass ran (None if this is the first).
    pub last_think_at: Option<DateTime<Utc>>,
}

/// Actions the orchestrator can request from its think() pass.
///
/// The run loop interprets these and executes them against the server.
/// This keeps the orchestrator pure — it returns intentions, not side effects.
#[derive(Debug, Clone)]
pub enum OrchestratorAction {
    /// Emit a thought to the orchestrator narration feed (stream of consciousness).
    EmitThought(String),
    /// Change a task's state.
    UpdateTaskState {
        task_id: String,
        state: TaskState,
    },
    /// Request a task be dispatched with priority.
    PrioritizeTask {
        task_id: String,
        reason: String,
    },
    // Future: DispatchAgent { task_id: String, config: DispatchConfig }
    // Future: CreateIssue { repo: String, title: String, body: String }
    // Future: CommentOnPr { pr_url: String, body: String }
}
