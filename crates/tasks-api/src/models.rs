//! Core domain types.
//!
//! The wire enums here deliberately carry an inherent `from_str -> Option`
//! rather than implementing `std::str::FromStr`: the callers (store row
//! mappers, API handlers) want an Option to turn into a typed BadEnum error,
//! not a FromStr::Err, and `parse()` inference noise buys nothing at this
//! size. Suppressed module-wide because every wire enum hits the same lint.
#![allow(clippy::should_implement_trait)]

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a fresh random id. Deliberately no `Default`: a default
            /// that returns a different value every call would be a lie —
            /// two "default" ids must not silently differ.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::new_v4().simple()))
            }

            pub fn from_raw(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

id_newtype!(ProjectId, "proj");
id_newtype!(TaskId, "task");
id_newtype!(SessionId, "sess");
id_newtype!(SpecId, "spec");
id_newtype!(BuildId, "build");

/// A repo being tracked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub repo_owner: String,
    pub repo_name: String,
    pub added_at: DateTime<Utc>,
}

/// A unit of work, sourced from a GitHub issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub gh_issue_number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub gh_state: GhState,
    pub state: TaskState,
    pub priority: i32,
    /// Position in the human-curated queue, 1-based. `None` means unranked and
    /// sorts after every ranked task. Only ever written by explicit reorder
    /// requests — never by the GitHub poller.
    pub manual_rank: Option<i32>,
    /// Consecutive failed scout dispatches. Persisted so restarts can't reset
    /// the count: at `MAX_DISPATCH_ATTEMPTS` the dispatcher rejects the task
    /// instead of retrying it forever. Cleared when a scout produces a spec.
    /// Never written by the GitHub poller.
    pub dispatch_attempts: u32,
    pub ingested_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhState {
    Open,
    Closed,
}

impl GhState {
    pub fn as_str(&self) -> &'static str {
        match self {
            GhState::Open => "open",
            GhState::Closed => "closed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(GhState::Open),
            "closed" => Some(GhState::Closed),
            _ => None,
        }
    }
}

/// Where a task sits in our Diamond 1 pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// One state per task, one list per state. `Backlog` is not work — it's the
/// ingested mirror of the repo's open issues. Everything from `Queued` onward
/// is work a human explicitly picked up, and it *stays* picked up: scout
/// failures and `needs_revision` verdicts return to `Queued`, never `Backlog`.
pub enum TaskState {
    /// Ingested from GitHub, untouched. Shown in the Tasks table, never queued
    /// or dispatched.
    Backlog,
    /// Explicitly picked up; in the scout queue, ordered by `manual_rank`.
    Queued,
    /// A Scout is actively exploring.
    Scouting,
    /// A spec has been produced and awaits a review verdict.
    InReview,
    /// Spec approved; parked until a Builder run consumes it.
    ReadyToBuild,
    /// A Builder run is implementing this task's spec (possibly batched with
    /// others). Failure returns to `ReadyToBuild` — the spec is still good.
    Building,
    /// Completed through the pipeline.
    Done,
    /// Rejected and won't be pursued.
    Rejected,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Backlog => "backlog",
            TaskState::Queued => "queued",
            TaskState::Scouting => "scouting",
            TaskState::InReview => "in_review",
            TaskState::ReadyToBuild => "ready_to_build",
            TaskState::Building => "building",
            TaskState::Done => "done",
            TaskState::Rejected => "rejected",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "backlog" => Some(TaskState::Backlog),
            "queued" => Some(TaskState::Queued),
            "scouting" => Some(TaskState::Scouting),
            "in_review" => Some(TaskState::InReview),
            "ready_to_build" => Some(TaskState::ReadyToBuild),
            "building" => Some(TaskState::Building),
            "done" => Some(TaskState::Done),
            "rejected" => Some(TaskState::Rejected),
            _ => None,
        }
    }
}

/// A Scout run — one VM executing a throwaway implementation for a task.
///
/// Not `Eq`: `usage` carries `f64` costs. `PartialEq` is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub task_id: TaskId,
    pub vm_id: Option<String>,
    pub branch: String,
    pub status: SessionStatus,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub exit_reason: Option<String>,
    /// Tokens and cost parsed from the agent's final stream-json `result`
    /// record. `None` for sessions that predate transcript capture, that
    /// never reached a result, or whose record didn't parse.
    #[serde(default)]
    pub usage: Option<SessionUsage>,
}

/// What one agent run cost. Every field is optional: the shape belongs to
/// Claude Code, and a renamed key must cost us a null, not a failed scout.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u64>,
}

/// One line of agent output, as persisted. `seq` is dense per session and
/// assigned at persist time, so readers can page and tail with `?since=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub session_id: SessionId,
    pub seq: i64,
    pub timestamp: DateTime<Utc>,
    pub stream: TranscriptStream,
    pub line: String,
}

/// Which pipe a transcript line came from. Mirrors `tasks_protocol::LogStream`
/// at the domain layer so the store doesn't depend on the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStream {
    Stdout,
    Stderr,
}

impl TranscriptStream {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranscriptStream::Stdout => "stdout",
            TranscriptStream::Stderr => "stderr",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stdout" => Some(TranscriptStream::Stdout),
            "stderr" => Some(TranscriptStream::Stderr),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    ScoutSucceeded,
    ScoutFailed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::ScoutSucceeded => "scout_succeeded",
            SessionStatus::ScoutFailed => "scout_failed",
            SessionStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(SessionStatus::Running),
            "scout_succeeded" => Some(SessionStatus::ScoutSucceeded),
            "scout_failed" => Some(SessionStatus::ScoutFailed),
            "cancelled" => Some(SessionStatus::Cancelled),
            _ => None,
        }
    }
}

/// The distilled artifact a Scout produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub id: SpecId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub content: String,
    pub complexity: Complexity,
    pub files_touched: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Simple,
    Medium,
    Complex,
}

impl Complexity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Complexity::Simple => "simple",
            Complexity::Medium => "medium",
            Complexity::Complex => "complex",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "simple" => Some(Complexity::Simple),
            "medium" => Some(Complexity::Medium),
            "complex" => Some(Complexity::Complex),
            _ => None,
        }
    }
}

/// A spec paired with the verdict a reviewer rendered on it. They always
/// travel together — feedback is unusable without the artifact it refers to —
/// so a re-scout carries both forward or neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedSpec {
    pub spec: Spec,
    pub status: SpecQueueStatus,
    pub feedback: Option<String>,
}

/// An entry in the spec queue — the orchestrator's working set for judging
/// what should be implemented next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecQueueEntry {
    pub spec_id: SpecId,
    pub status: SpecQueueStatus,
    pub rank: Option<i32>,
    pub approved_at: Option<DateTime<Utc>>,
    pub feedback: Option<String>,
    pub blocking_dependencies: Vec<TaskId>,
}

/// A spec queue entry joined with the id of the task the spec was written for.
/// The queue listing needs the task id, and it lives on `specs`, not `spec_queue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecQueueItem {
    #[serde(flatten)]
    pub entry: SpecQueueEntry,
    pub task_id: TaskId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecQueueStatus {
    PendingReview,
    Approved,
    NeedsRevision,
    Blocked,
    Rejected,
    /// Consumed by a successful Builder run. Terminal, and system-assigned:
    /// `review_spec` rejects it — it is how the approved queue drains, not a
    /// verdict a reviewer can render.
    Built,
}

impl SpecQueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SpecQueueStatus::PendingReview => "pending_review",
            SpecQueueStatus::Approved => "approved",
            SpecQueueStatus::NeedsRevision => "needs_revision",
            SpecQueueStatus::Blocked => "blocked",
            SpecQueueStatus::Rejected => "rejected",
            SpecQueueStatus::Built => "built",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending_review" => Some(SpecQueueStatus::PendingReview),
            "approved" => Some(SpecQueueStatus::Approved),
            "needs_revision" => Some(SpecQueueStatus::NeedsRevision),
            "blocked" => Some(SpecQueueStatus::Blocked),
            "rejected" => Some(SpecQueueStatus::Rejected),
            "built" => Some(SpecQueueStatus::Built),
            _ => None,
        }
    }
}

/// Global operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Play,
    Pause,
    Stop,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Play => "play",
            Mode::Pause => "pause",
            Mode::Stop => "stop",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "play" => Some(Mode::Play),
            "pause" => Some(Mode::Pause),
            "stop" => Some(Mode::Stop),
            _ => None,
        }
    }
}

/// One serial Builder run over a *set* of approved specs, producing one
/// branch and one PR. Set-shaped from day one (`build_specs`), even though v0
/// is usually invoked with a single spec.
///
/// Everything here is Tasks-owned or immutable. `pr_number` is an identifier,
/// never a state: PR mergeability, checks, and open/closed are GitHub's and
/// are queried at decision time, not stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub id: BuildId,
    pub project_id: ProjectId,
    pub vm_id: Option<String>,
    /// Branch name the server pushes, chosen at request time: `build/<id>`.
    pub branch: String,
    pub base_branch: String,
    /// Commit the branch grew from — the bundle's prerequisite. Immutable.
    pub base_sha: Option<String>,
    /// Branch tip the VM reported and the server verified before pushing.
    pub head_sha: Option<String>,
    pub pr_number: Option<u64>,
    pub status: BuildStatus,
    /// SUMMARY.md from the agent (the PR body), if it wrote one.
    pub summary: Option<String>,
    pub files_touched: Vec<String>,
    pub exit_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    /// Requested, waiting for the serial build loop to claim it.
    Queued,
    /// Claimed; a Builder VM is (or is about to be) running it.
    Running,
    /// Branch pushed and PR opened.
    Succeeded,
    /// Any failure — agent, egress, push, or PR. `exit_reason` says which.
    Failed,
}

impl BuildStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuildStatus::Queued => "queued",
            BuildStatus::Running => "running",
            BuildStatus::Succeeded => "succeeded",
            BuildStatus::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(BuildStatus::Queued),
            "running" => Some(BuildStatus::Running),
            "succeeded" => Some(BuildStatus::Succeeded),
            "failed" => Some(BuildStatus::Failed),
            _ => None,
        }
    }
}

/// One turn in the orchestrator conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorMessage {
    pub seq: i64,
    pub role: ChatRole,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
    /// An automated pipeline notification injected into the conversation
    /// (spec landed, build finished, tasks ingested). Counts as unanswered
    /// input like a user turn — the orchestrator reacts to it proactively.
    Event,
    /// A server-written note about the conversation itself — currently only
    /// the seam where one Claude Code session ended and another began.
    ///
    /// Deliberately *not* input: the durable transcript looks continuous
    /// across a boundary the agent no longer remembers, so the reader needs
    /// to see it, but making the orchestrator answer "you just restarted"
    /// would spend a turn acknowledging its own amnesia.
    System,
}

impl ChatRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::Event => "event",
            ChatRole::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(ChatRole::User),
            "assistant" => Some(ChatRole::Assistant),
            "event" => Some(ChatRole::Event),
            "system" => Some(ChatRole::System),
            _ => None,
        }
    }

    /// Whether a turn with this role is *input* the orchestrator owes a reply
    /// to. The tick condition is built on this, so it is the one place the
    /// answer lives.
    pub fn is_input(&self) -> bool {
        matches!(self, ChatRole::User | ChatRole::Event)
    }
}

/// The three Home briefing slots. Order here is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BriefingSection {
    /// Active or quiet, what meta-threads are being tackled, where the
    /// bottleneck is.
    StateOfProject,
    /// Open PRs and their real states, staleness, cleanup, risky overlaps.
    Changes,
    /// Incoming issues, inferred priority, issue health, what to queue next.
    Issues,
}

impl BriefingSection {
    pub const ALL: [BriefingSection; 3] = [
        BriefingSection::StateOfProject,
        BriefingSection::Changes,
        BriefingSection::Issues,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            BriefingSection::StateOfProject => "state_of_project",
            BriefingSection::Changes => "changes",
            BriefingSection::Issues => "issues",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "state_of_project" => Some(BriefingSection::StateOfProject),
            "changes" => Some(BriefingSection::Changes),
            "issues" => Some(BriefingSection::Issues),
            _ => None,
        }
    }
}

/// One generated briefing, as persisted. A cache with a visible date: the
/// GitHub facts inside `content` were queried at `generated_at` and are never
/// read back as truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Briefing {
    pub section: BriefingSection,
    pub content: String,
    pub generated_at: DateTime<Utc>,
    /// Newest event seq when generation started — for later regen gating.
    pub event_high_water: i64,
}

/// The orchestrator's Claude Code session as clients see it
/// (`GET /orchestrator/session`): enough to resume it interactively
/// (`cd <workdir> && claude --resume <cc_session_id>`) and whether someone
/// already has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorSessionInfo {
    /// `None` until the first tick creates the session.
    pub cc_session_id: Option<String>,
    /// The agent's working directory, written at server startup.
    pub workdir: Option<String>,
    /// A human holds an interactive checkout (fresh heartbeat); headless
    /// ticks are suspended while true.
    pub checked_out: bool,
    /// Size of the current session's context as of its last turn, in tokens.
    /// `None` before the session has taken a turn, or when the agent isn't
    /// emitting stream-json usage (plain-text agents, test stubs).
    pub context_tokens: Option<i64>,
}

/// Something the pipeline is owed, computed from its state.
///
/// Obligations are never stored: they exist for exactly as long as the state
/// that implies them, and disappear when the work is done rather than when
/// someone is told about it. That is the difference between a notification
/// and an obligation, and it is why a dropped tick can no longer strand a
/// spec — nothing was consumed, so the next pass sees the same thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligation {
    pub kind: ObligationKind,
    /// The spec, build, or task the obligation is about.
    pub subject_id: String,
    /// One human-readable line, for the turn that surfaces it.
    pub summary: String,
    /// When the state implying this obligation came about.
    pub since: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationKind {
    /// A spec is waiting for a verdict and no decision has been recorded.
    ReviewSpec,
    /// A batch burned through its build attempts and stopped. Nothing will
    /// pick it up again until someone decides what to do.
    UnblockSpec,
}

impl ObligationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObligationKind::ReviewSpec => "review_spec",
            ObligationKind::UnblockSpec => "unblock_spec",
        }
    }
}

/// Who caused a state change.
///
/// The API is loopback and unauthenticated by design, so this is attribution,
/// not authentication: the orchestrator proves itself by presenting a token
/// the server minted for it, which stops it *accidentally* passing as the
/// human but would not stop a local process forging either identity.
///
/// It is load-bearing anyway, and not only for the audit trail — the
/// orchestrator must not be notified about its own actions, and that filter
/// is only as correct as this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The default: anything that did not prove otherwise.
    #[default]
    Human,
    Orchestrator,
}

impl Actor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Actor::Human => "human",
            Actor::Orchestrator => "orchestrator",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "human" => Some(Actor::Human),
            "orchestrator" => Some(Actor::Orchestrator),
            _ => None,
        }
    }
}

/// What a decision did. Narrower than [`SpecQueueStatus`] on purpose: only
/// transitions someone *chose* are decisions, so `built` (assigned by a
/// successful Builder run) is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAction {
    Approve,
    NeedsRevision,
    Reject,
    RequestBuild,
}

impl DecisionAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionAction::Approve => "approve",
            DecisionAction::NeedsRevision => "needs_revision",
            DecisionAction::Reject => "reject",
            DecisionAction::RequestBuild => "request_build",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(DecisionAction::Approve),
            "needs_revision" => Some(DecisionAction::NeedsRevision),
            "reject" => Some(DecisionAction::Reject),
            "request_build" => Some(DecisionAction::RequestBuild),
            _ => None,
        }
    }
}

/// One entry in the append-only decisions ledger.
///
/// The ledger indexes the conversation rather than replacing it: the
/// orchestrator's reasoning stays prose in `orchestrator_messages`, and
/// `transcript_seq` says where. That is the deal a long-lived accumulating
/// session forces — a verdict that depended on the whole conversation cannot
/// be replayed, so it has to be readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub seq: i64,
    pub subject_kind: String,
    pub subject_id: String,
    pub action: DecisionAction,
    pub actor: Actor,
    pub rationale: Option<String>,
    pub evidence: Option<serde_json::Value>,
    /// The orchestrator turn carrying the reasoning. `None` for human
    /// verdicts, and briefly `None` for an orchestrator's own until the turn
    /// it was made during finishes.
    pub transcript_seq: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// The decision behind a state change, supplied by whoever made it.
///
/// Threaded into the store alongside the change itself so the ledger row and
/// the state it explains commit together — a decision that can go missing is
/// not an audit trail.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DecisionInput {
    pub actor: Actor,
    pub rationale: Option<String>,
    pub evidence: Option<serde_json::Value>,
}

impl DecisionInput {
    /// A human acting with no stated reason — the common case from the app,
    /// and the default for anything that did not identify itself.
    pub fn human() -> Self {
        Self::default()
    }
}

/// Why an orchestrator session stopped being the live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    /// `--resume` failed and the context was lost involuntarily. The chat
    /// projection survives; the agent's memory does not.
    ResumeFailed,
    /// Deliberately retired — context pressure, seeded forward on our terms.
    Rotated,
}

impl SessionEndReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionEndReason::ResumeFailed => "resume_failed",
            SessionEndReason::Rotated => "rotated",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "resume_failed" => Some(SessionEndReason::ResumeFailed),
            "rotated" => Some(SessionEndReason::Rotated),
            _ => None,
        }
    }
}

/// One Claude Code session the orchestrator has lived in. The ledger these
/// rows form is what makes context loss legible: the chat projection is
/// continuous across boundaries the agent itself does not survive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorSession {
    pub cc_session_id: String,
    pub started_at: DateTime<Utc>,
    /// `None` for the live session.
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<SessionEndReason>,
    /// Context size at this session's last turn — the reading a rotation
    /// threshold is compared against.
    pub last_context_tokens: Option<i64>,
    /// The continuation note this session was seeded into its successor
    /// with. Unwritten until owned rotation lands.
    pub summary: Option<String>,
    pub summary_generated_at: Option<DateTime<Utc>>,
}

/// One moment of an in-flight orchestrator tick, streamed over
/// `GET /orchestrator/stream`. Ephemeral by design: nothing here is
/// persisted — the durable record is the finished message in
/// `orchestrator_messages` (and Claude Code's own session transcript).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrchestratorFeedEvent {
    /// A chunk of assistant text, in generation order.
    Delta { text: String },
    /// The agent invoked a tool (e.g. a curl against this API).
    Tool { label: String },
    /// The tick finished; fetch `/orchestrator/messages` for the reply.
    Done,
}
