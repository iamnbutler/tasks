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

/// One line of agent output, as persisted. `seq` is dense **per owner** and
/// assigned at persist time, so readers can page and tail with `?since=`. A
/// client paging several runs must keep one cursor per owner: a build's first
/// line is seq 1 no matter what its specs' scout sessions recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub owner: TranscriptOwner,
    pub seq: i64,
    pub timestamp: DateTime<Utc>,
    pub stream: TranscriptStream,
    pub line: String,
}

/// Which run produced a transcript line. Two variants rather than one opaque
/// id because they are two resources behind two routes — a reader holding a
/// line should not have to guess which one to fetch more from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptOwner {
    Session { session_id: SessionId },
    Build { build_id: BuildId },
}

impl TranscriptOwner {
    pub fn session(id: &SessionId) -> Self {
        TranscriptOwner::Session {
            session_id: id.clone(),
        }
    }

    pub fn build(id: &BuildId) -> Self {
        TranscriptOwner::Build {
            build_id: id.clone(),
        }
    }

    /// The owning row's id, whichever side of the arc is set.
    pub fn id(&self) -> &str {
        match self {
            TranscriptOwner::Session { session_id } => session_id.as_str(),
            TranscriptOwner::Build { build_id } => build_id.as_str(),
        }
    }
}

impl fmt::Display for TranscriptOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
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
    /// When the Builder agent phase ended — the drain concluding, before VM
    /// teardown and before the push and PR. This, minus `started_at`, is the
    /// interval the run budget bounds, and the one to render as "took".
    pub agent_finished_at: Option<DateTime<Utc>>,
    /// When the build row reached its terminal state: after teardown, and on
    /// success after the branch was pushed and the PR opened. Always at or
    /// after `agent_finished_at`.
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
    /// How much context the session is holding as of its last turn, in
    /// tokens: the input side (fresh + cached) of the prompt behind its last
    /// main-chain model call. An absolute reading — this is the number to
    /// compare against a context window. `None` before the session has taken
    /// a turn, or when the agent isn't emitting stream-json usage
    /// (plain-text agents, test stubs).
    pub context_tokens: Option<i64>,
    /// What the last tick *spent*, in tokens: the invocation's aggregate over
    /// every internal turn, each of which re-read the cached prefix. This is
    /// a bill, and it routinely exceeds the context window several times
    /// over — never compare it against one, and never render it as "context
    /// used".
    pub tick_tokens: Option<i64>,
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
    /// A spec is approved and no build is carrying it. Approval is not
    /// delivery: without this, `dispatch_builds` is permission with nothing to
    /// prompt it, and approved work sits still until a human notices.
    DispatchBuild,
    /// A batch burned through its build attempts and stopped. Nothing will
    /// pick it up again until someone decides what to do.
    UnblockSpec,
}

impl ObligationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObligationKind::ReviewSpec => "review_spec",
            ObligationKind::DispatchBuild => "dispatch_build",
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
    /// A task was moved from the backlog into the queue, where a Scout will
    /// pick it up. Recorded because it is where spend begins.
    QueueTask,
    /// An issue was filed — work that would otherwise have been lost.
    CaptureWork,
    /// An issue was closed: finished, or judged no longer worth doing.
    RetireWork,
    /// A closed issue was reopened — the recourse for a retirement that was
    /// wrong, and the reason `retire_work` can be trusted with `live`.
    ReopenWork,
    /// Something was said on an issue or a pull request. The lightest write
    /// here: recorded because a comment is still the system speaking in
    /// public under the owner's name.
    CommentOnWork,
    /// A pull request was merged. The only action whose recourse is a revert
    /// rather than an edit.
    MergeBuild,
    /// A pull request was closed unmerged — the branch is not going to land.
    AbandonBuild,
    /// A comment pinned to a line of a pull request's diff. Separate from
    /// `CommentOnWork` because it points at code rather than at the thread,
    /// and because it can be wrong about a line that no longer exists.
    ReviewComment,
    /// An issue's body was rewritten. The only action here that destroys
    /// rather than appends, which is why its ledger row carries the previous
    /// text: the thing worth auditing is the diff, not the event.
    EditIssue,
    /// An issue's labels were replaced.
    LabelIssue,
}

impl DecisionAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionAction::Approve => "approve",
            DecisionAction::NeedsRevision => "needs_revision",
            DecisionAction::Reject => "reject",
            DecisionAction::RequestBuild => "request_build",
            DecisionAction::QueueTask => "queue_task",
            DecisionAction::CaptureWork => "capture_work",
            DecisionAction::RetireWork => "retire_work",
            DecisionAction::ReopenWork => "reopen_work",
            DecisionAction::CommentOnWork => "comment_on_work",
            DecisionAction::MergeBuild => "merge_build",
            DecisionAction::AbandonBuild => "abandon_build",
            DecisionAction::ReviewComment => "review_comment",
            DecisionAction::EditIssue => "edit_issue",
            DecisionAction::LabelIssue => "label_issue",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(DecisionAction::Approve),
            "needs_revision" => Some(DecisionAction::NeedsRevision),
            "reject" => Some(DecisionAction::Reject),
            "request_build" => Some(DecisionAction::RequestBuild),
            "queue_task" => Some(DecisionAction::QueueTask),
            "capture_work" => Some(DecisionAction::CaptureWork),
            "retire_work" => Some(DecisionAction::RetireWork),
            "reopen_work" => Some(DecisionAction::ReopenWork),
            "comment_on_work" => Some(DecisionAction::CommentOnWork),
            "merge_build" => Some(DecisionAction::MergeBuild),
            "abandon_build" => Some(DecisionAction::AbandonBuild),
            "review_comment" => Some(DecisionAction::ReviewComment),
            "edit_issue" => Some(DecisionAction::EditIssue),
            "label_issue" => Some(DecisionAction::LabelIssue),
            _ => None,
        }
    }
}

/// Something the orchestrator can do without being asked.
///
/// Deliberately five separate switches rather than one autonomy dial. The
/// axis that matters is reversibility, and it does not line up with how
/// dramatic a capability sounds: filing an issue is undone by closing it,
/// while auto-approving a spec costs a Builder run and a PR someone has to
/// read. `dispatch_builds` live with `auto_review_specs` in shadow is a
/// coherent and probably desirable state that a single play/pause switch
/// cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// File an issue for discovered work.
    CaptureWork,
    /// Close an issue: done, or no longer worth doing.
    RetireWork,
    /// Move a task from backlog into the queue, where a Scout will pick it up.
    QueueTasks,
    /// Batch approved specs into a Builder run.
    DispatchBuilds,
    /// Render a review verdict on a spec.
    AutoReviewSpecs,
    /// Say something on an issue or a pull request.
    CommentOnWork,
    /// Decide a Builder PR's fate: merge it, or close it unmerged.
    /// `dispatch_builds` starts the run; this one finishes it.
    LandBuilds,
    /// Revise work already filed: rewrite an issue's body, change its labels.
    /// Separate from `CaptureWork` because it rewrites rather than appends —
    /// a bad capture leaves a bad issue, a bad edit destroys a good one.
    CurateWork,
}

impl Capability {
    /// Every capability, in the order the charter is meant to be flipped:
    /// additive and trivially reversible first, irreversible-ish last.
    pub const ALL: [Capability; 8] = [
        Capability::CaptureWork,
        Capability::CommentOnWork,
        Capability::RetireWork,
        Capability::QueueTasks,
        Capability::DispatchBuilds,
        Capability::AutoReviewSpecs,
        Capability::LandBuilds,
        Capability::CurateWork,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::CaptureWork => "capture_work",
            Capability::RetireWork => "retire_work",
            Capability::QueueTasks => "queue_tasks",
            Capability::DispatchBuilds => "dispatch_builds",
            Capability::AutoReviewSpecs => "auto_review_specs",
            Capability::CommentOnWork => "comment_on_work",
            Capability::LandBuilds => "land_builds",
            Capability::CurateWork => "curate_work",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "capture_work" => Some(Capability::CaptureWork),
            "retire_work" => Some(Capability::RetireWork),
            "queue_tasks" => Some(Capability::QueueTasks),
            "dispatch_builds" => Some(Capability::DispatchBuilds),
            "auto_review_specs" => Some(Capability::AutoReviewSpecs),
            "comment_on_work" => Some(Capability::CommentOnWork),
            "land_builds" => Some(Capability::LandBuilds),
            "curate_work" => Some(Capability::CurateWork),
            _ => None,
        }
    }

    /// One line for the generated authority section of the system prompt.
    pub fn describe(&self) -> &'static str {
        match self {
            Capability::CaptureWork => "file issues for work you discover",
            Capability::RetireWork => "close issues that are done or no longer worth doing",
            Capability::QueueTasks => "move tasks from the backlog into the queue",
            Capability::DispatchBuilds => "batch approved specs into Builder runs",
            Capability::AutoReviewSpecs => "render review verdicts on specs",
            Capability::CommentOnWork => "comment on issues and pull requests",
            Capability::LandBuilds => "merge a Builder's pull request, or close it unmerged",
            Capability::CurateWork => "revise an issue you filed: its body, its labels",
        }
    }
}

/// How much of a capability the orchestrator actually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CharterLevel {
    /// Refused at the endpoint. The zero value, so an unreadable or absent
    /// charter fails closed — but not where anything starts: the seed is
    /// `Live` across the board.
    #[default]
    Off,
    /// The call is accepted, the decision is recorded with `enforced = 0`, and
    /// nothing happens.
    ///
    /// Narrated, not silent: the orchestrator explains its reasoning in the
    /// conversation as it always does, and the ledger keeps the verdict it
    /// would have rendered.
    ///
    /// Not a probation period on the way to `Live`, which is what it was
    /// originally for and what it is bad at — the capability does the entire
    /// job and then hands the answer back to be re-entered by hand, spending
    /// the human attention the system exists to save. Reach for it to demote
    /// a capability that has already misbehaved and whose reasoning you want
    /// to keep watching.
    Shadow,
    /// Applied.
    Live,
}

impl CharterLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            CharterLevel::Off => "off",
            CharterLevel::Shadow => "shadow",
            CharterLevel::Live => "live",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "off" => Some(CharterLevel::Off),
            "shadow" => Some(CharterLevel::Shadow),
            "live" => Some(CharterLevel::Live),
            _ => None,
        }
    }
}

/// One capability's standing, as the charter records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharterEntry {
    pub capability: Capability,
    pub level: CharterLevel,
    /// Optional manual brake: actions of this kind per day. `None` — the
    /// default for every capability — is uncapped.
    ///
    /// Deliberately not part of the design's safety story. Runaway protection
    /// lives in the pipeline's shape (builds are serial, scouts are bounded by
    /// `SCOUT_MAX_CONCURRENT`, a failing batch is retired by the attempt cap),
    /// and the point of this system is that work moves without being asked.
    /// This exists for the narrow case of a capability caught misbehaving,
    /// where the alternative is turning it off entirely.
    pub daily_limit: Option<i64>,
    pub updated_at: DateTime<Utc>,
}

/// Why work is being retired, mapped onto GitHub's `state_reason`.
///
/// The two are not variations on a theme. "Completed" has a cheap evidence
/// standard — a merged PR or a named commit, queried live — and the
/// orchestrator has already been wrong by asserting opened PRs as shipped
/// work. "Not planned" is a recalibration judgment with no such standard, and
/// it is the more valuable of the two precisely because nobody ever gets round
/// to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Completed,
    NotPlanned,
}

impl CloseReason {
    /// GitHub's `state_reason` spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            CloseReason::Completed => "completed",
            CloseReason::NotPlanned => "not_planned",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "completed" => Some(CloseReason::Completed),
            "not_planned" => Some(CloseReason::NotPlanned),
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
    /// Whether the state actually changed. `false` for a shadow decision: the
    /// judgment is real and recorded, the effect never happened. Reading the
    /// two as one would turn an evaluation into a history.
    #[serde(default = "crate::models::default_true")]
    pub enforced: bool,
    pub created_at: DateTime<Utc>,
}

pub(crate) fn default_true() -> bool {
    true
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
    /// Context this session was holding at its last turn: the input side of
    /// the prompt behind that turn's last main-chain model call. The reading
    /// a rotation threshold is compared against. `None` for sessions whose
    /// turns all predate the measurement — the column that used to hold this
    /// name held tick spend, and those values live on in `last_tick_tokens`
    /// rather than being reinterpreted here.
    pub last_context_tokens: Option<i64>,
    /// What this session's last tick spent: the invocation's aggregate across
    /// its internal turns. A cost signal — useful for a budget or an
    /// "expensive tick" alert, and never to be compared against a context
    /// window or used for a compaction decision.
    pub last_tick_tokens: Option<i64>,
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
