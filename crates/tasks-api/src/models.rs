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

/// How much of the pipeline still runs for one repo — the honest per-repo
/// counterpart to the global [`Mode`].
///
/// One ordered column rather than a pair of booleans: `paused` and `archived`
/// are not orthogonal (archived already implies not dispatching), so two flags
/// would have a meaningless fourth state and un-archiving would have to
/// remember which switch it was allowed to touch. The order is how much of the
/// pipeline survives:
///
/// | | issues ingested | scouts / builds dispatched |
/// | --- | --- | --- |
/// | `active` | yes | yes |
/// | `paused` | yes | no |
/// | `archived` | no | no |
///
/// Like [`Mode`], it gates **new** work only: nothing here interrupts a scout
/// or a build already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    #[default]
    Active,
    Paused,
    Archived,
}

impl ProjectStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProjectStatus::Active => "active",
            ProjectStatus::Paused => "paused",
            ProjectStatus::Archived => "archived",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(ProjectStatus::Active),
            "paused" => Some(ProjectStatus::Paused),
            "archived" => Some(ProjectStatus::Archived),
            _ => None,
        }
    }

    /// Whether the dispatcher may start new scouts and builds for this repo.
    pub fn dispatches(&self) -> bool {
        matches!(self, ProjectStatus::Active)
    }

    /// Whether the poller may ingest this repo's issues as new tasks.
    ///
    /// Only the *upsert* half of a poll asks this. Closure is learned from
    /// absence in the open set, so an archived project keeps being fetched —
    /// otherwise every task it already has would sit at `gh_state = open`
    /// forever, and a Builder PR it already opened would never be resolved.
    pub fn ingests(&self) -> bool {
        !matches!(self, ProjectStatus::Archived)
    }
}

impl fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A repo being tracked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub repo_owner: String,
    pub repo_name: String,
    pub added_at: DateTime<Utc>,
    /// `#[serde(default)]` so a client built from this repo can read a server
    /// that predates the column: absent reads as `active`, which is what every
    /// project was before there was a switch.
    #[serde(default)]
    pub status: ProjectStatus,
}

impl Project {
    /// `owner/repo`, the way a human names it.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.repo_owner, self.repo_name)
    }
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
    /// Staged instructions for the *next* Scout run on this task — what to
    /// look at, what not to bother with, a constraint the issue does not
    /// state. Copied onto the [`Session`] at dispatch and **not cleared**: a
    /// VM death or a `needs_revision` return would otherwise leave the retry
    /// unaimed with nobody noticing, and visible persistence beats silent
    /// loss. Never written by the GitHub poller.
    #[serde(default)]
    pub scout_directions: Option<Directions>,
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
    /// The Builder's pull request is open and nobody has resolved it yet.
    ///
    /// Live work, not history: a PR that opened is a claim, not a delivery.
    /// The poller reads the PR at decision time and either retires the task
    /// (merged → the issue is closed as completed → `Done` on the next pass)
    /// or returns the batch to `ReadyToBuild` (closed unmerged).
    AwaitingMerge,
    /// Completed through the pipeline — which here means exactly one thing:
    /// the GitHub issue is closed upstream. Written only by closure-derived
    /// retirement, never by a build.
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
            TaskState::AwaitingMerge => "awaiting_merge",
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
            "awaiting_merge" => Some(TaskState::AwaitingMerge),
            "done" => Some(TaskState::Done),
            "rejected" => Some(TaskState::Rejected),
            _ => None,
        }
    }

    /// Whether the pipeline is finished with this task. Nothing here is live
    /// work — `awaiting_merge` deliberately is not terminal, because the
    /// poller is still driving it.
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskState::Done | TaskState::Rejected)
    }

    /// A leading `ORDER BY` term that sorts terminal states last, for the
    /// listing queries that share one ordering.
    ///
    /// It lives next to [`TaskState::is_terminal`] because SQLite cannot call
    /// that method and the two must agree; the unit test below iterates every
    /// variant and fails when they drift.
    pub const ORDER_TERMINAL_LAST_SQL: &'static str =
        "CASE WHEN state IN ('done', 'rejected') THEN 1 ELSE 0 END";
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
    /// What this run was told, as it was told it — a *copy* of the task's
    /// staged [`Task::scout_directions`] taken at dispatch, not a reference to
    /// them. The task can be re-aimed tomorrow; the run's record must keep
    /// saying what *this* run was asked to do.
    #[serde(default)]
    pub directions: Option<Directions>,
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
    /// The run ended without a spec but left notes behind. A third terminal
    /// outcome, neither success nor failure: there is no [`Spec`] row, no
    /// queue entry and no review path — the salvage's only consumer is the
    /// next attempt's prompt. See [`ScoutNotes`].
    ScoutStoppedEarly,
    /// The run ended with nothing to salvage.
    ScoutFailed,
    /// Stopped on purpose, by an accountable actor, while it was still
    /// running. Never a verdict on the work: `exit_reason` names who asked and
    /// why, the attempt count is untouched, and whatever the run had
    /// checkpointed is kept — see [`ScoutNotes`].
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::ScoutSucceeded => "scout_succeeded",
            SessionStatus::ScoutStoppedEarly => "scout_stopped_early",
            SessionStatus::ScoutFailed => "scout_failed",
            SessionStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(SessionStatus::Running),
            "scout_succeeded" => Some(SessionStatus::ScoutSucceeded),
            "scout_stopped_early" => Some(SessionStatus::ScoutStoppedEarly),
            "scout_failed" => Some(SessionStatus::ScoutFailed),
            "cancelled" => Some(SessionStatus::Cancelled),
            _ => None,
        }
    }
}

/// What a scout run had written down when it was interrupted — **never a
/// spec**.
///
/// One row per session, superseded by each checkpoint. Deliberately not a
/// column on `sessions` (a quarter-megabyte per interrupted run would ride
/// every `GET /sessions`) and deliberately not a [`Spec`] (there must be no
/// shape in which salvage reaches a reviewer). Its only consumer is the next
/// attempt's prompt, where it is quoted as an explicitly unverified lead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutNotes {
    pub session_id: SessionId,
    pub task_id: TaskId,
    /// Why the run ended. `None` while the run is still going — a checkpoint
    /// is written before anyone knows how it ends.
    pub reason: Option<String>,
    pub notes: String,
    pub files_touched: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

/// The distilled artifact a Scout produces — or, rarely, one a human wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub id: SpecId,
    /// The Scout run this came out of, and **the provenance contract**: `None`
    /// means no Scout ever ran, because a human wrote the spec by hand through
    /// `POST /tasks/{id}/build-now` for a task whose issue body already was the
    /// specification.
    ///
    /// That is the only difference such a spec has. It carries no transcript,
    /// no session to link to, and no independent reviewer — the human who
    /// wrote it *is* the review — so a client that renders a scout link should
    /// say "human-authored" here rather than leave the absence to be inferred.
    #[serde(default)]
    pub session_id: Option<SessionId>,
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
    /// What whoever requested this build told the Builder to do, beyond the
    /// specs. Written in the same `INSERT` as the row — a build that existed
    /// without them would dispatch unaimed and silently — and never staged:
    /// unlike a task, a build is created for one run and never re-aimed.
    #[serde(default)]
    pub directions: Option<Directions>,
    /// What the project's own test suite said, run by the Builder **supervisor**
    /// inside the VM against the tree the bundle carries.
    ///
    /// `None` means no run is on record: a build that predates the check, or
    /// one whose supervisor image has not been rebuilt yet. Never green — see
    /// [`VerificationStatus::is_green`], which is the only way to ask.
    #[serde(default)]
    pub verification: Option<Verification>,
}

/// A build's verification, on the client wire.
///
/// The deliberate twin of `tasks_protocol::verify::Verification`, and separate
/// from it for the reason every type in this crate is: the VM wire decodes
/// *forgivingly*, because a terminal event that will not parse hangs a run
/// until its deadline, while clients ship from this repository, so skew here
/// should be a build error rather than a runtime fallback. `builder::api_verification`
/// is the seam, and it is where an unrecognised status becomes `None` rather
/// than a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub status: VerificationStatus,
    /// Prose for a human. **Nothing branches on it** — it names the gate that
    /// ruled and what happened, and that is all.
    #[serde(default)]
    pub detail: String,
}

impl Verification {
    /// Shorthand for [`VerificationStatus::is_green`].
    pub fn is_green(&self) -> bool {
        self.status.is_green()
    }
}

/// The states a build's verification can be in.
///
/// **There is deliberately no `Failed`**: a red suite fails the build inside
/// the VM before a bundle is packaged, so "shipped and red" is unrepresentable
/// rather than merely avoided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// The project's declared suite ran and passed. The only green state.
    Passed,
    /// The project declares no suite, or declares an empty one.
    Undeclared,
    /// Something below the suite failed: a runner that would not start, an
    /// unreadable script, an unstated budget, or a status this binary could
    /// not parse.
    Unavailable,
    /// The suite was killed by its budget, or there was not enough budget left
    /// to start it. The build still ships; the status is honestly not green.
    TimedOut,
}

impl VerificationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Undeclared => "undeclared",
            Self::Unavailable => "unavailable",
            Self::TimedOut => "timed_out",
        }
    }

    /// Strict, unlike the VM wire's: an unrecognised value is `None` here and
    /// the caller decides, rather than decaying silently.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "passed" => Some(Self::Passed),
            "undeclared" => Some(Self::Undeclared),
            "unavailable" => Some(Self::Unavailable),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }

    /// Whether a passing run of the project's own suite backs this build.
    ///
    /// The only way anything asks, so a variant added later cannot become
    /// green by omission.
    pub fn is_green(&self) -> bool {
        match self {
            Self::Passed => true,
            Self::Undeclared | Self::Unavailable | Self::TimedOut => false,
        }
    }
}

impl std::fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// Stopped on purpose while it was queued or running. Deliberately not a
    /// `Failed`: the batch's specs go back to `approved` without a strike, and
    /// a reader of the row can tell "somebody stopped this" from "this could
    /// not be built" — which is the whole point of the distinction.
    Cancelled,
}

impl BuildStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuildStatus::Queued => "queued",
            BuildStatus::Running => "running",
            BuildStatus::Succeeded => "succeeded",
            BuildStatus::Failed => "failed",
            BuildStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(BuildStatus::Queued),
            "running" => Some(BuildStatus::Running),
            "succeeded" => Some(BuildStatus::Succeeded),
            "failed" => Some(BuildStatus::Failed),
            "cancelled" => Some(BuildStatus::Cancelled),
            _ => None,
        }
    }

    /// Whether the build has reached a state nothing will move it out of.
    /// The predicate `POST /builds/{id}/cancel` refuses on.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BuildStatus::Succeeded | BuildStatus::Failed | BuildStatus::Cancelled
        )
    }
}

/// Which kind of run something is about — a Scout session or a Builder run.
///
/// The discriminant that lets one `cancellations` table and one cancel path
/// serve both dispatchers. Deliberately not folded into [`TranscriptOwner`]:
/// that one carries the id and exists to address a transcript, this one is the
/// bare kind and travels beside an id that is already in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Session,
    Build,
}

impl RunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunKind::Session => "session",
            RunKind::Build => "build",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "session" => Some(RunKind::Session),
            "build" => Some(RunKind::Build),
            _ => None,
        }
    }

    /// What a run of this kind is called in prose — for an `exit_reason` or a
    /// banner, where "session" is jargon and "scout" is the thing.
    pub fn noun(&self) -> &'static str {
        match self {
            RunKind::Session => "scout",
            RunKind::Build => "build",
        }
    }
}

impl fmt::Display for RunKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

/// An enrolled external agent: a name the human (or the orchestrator, under
/// `enroll_agents`) handed a short-lived code to, so its messages land in the
/// orchestrator conversation under that name instead of as the human's.
///
/// The code itself is never here — it is returned exactly once at mint time
/// and stored only as a SHA-256 hash, the same custody a broker lease gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEnrollment {
    pub id: i64,
    pub name: String,
    pub minted_by: Actor,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
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
    /// The model the session's main chain last ran on, as the agent's own wire
    /// id (`claude-opus-5[1m]`). Never derived from configuration:
    /// `ORCHESTRATOR_CMD` may name no model at all, and the agent's default is
    /// the agent's to choose.
    pub model_id: Option<String>,
    /// That model's context window, in tokens — the denominator
    /// [`Self::context_tokens`] is read against, as reported by the agent
    /// rather than as a table in our code. `None` until a tick has run under a
    /// model that reports one, and a client with no window must show the
    /// tokens alone rather than inventing a percentage.
    pub context_window: Option<i64>,
    /// How [`Self::context_tokens`] is made up. Same record, so the parts sum
    /// to the whole.
    pub context_breakdown: Option<ContextBreakdown>,
    /// How many times this session has been compacted, and when it last was.
    ///
    /// Compaction happens *inside* the agent and keeps the session id, so
    /// nothing else here distinguishes it from a turn that simply held less:
    /// the gauge just reads lower. This is the record that it fired.
    ///
    /// `0` means "none counted", which is not the same as "never happened" —
    /// a session that predates this counter carries its earlier compactions
    /// only in the server log. So a client shows the count when there is one
    /// and shows *nothing* when there isn't, rather than rendering a zero as
    /// "never".
    pub compactions: i64,
    pub last_compacted_at: Option<DateTime<Utc>>,
}

/// What a context reading is made of: the input side of one model call, split
/// by how each token was paid for.
///
/// The three sum to `context_tokens`. They are the same tokens as far as the
/// *window* is concerned — which is why the gauge adds them — and entirely
/// different tokens as far as the bill is concerned, which is why they are
/// kept apart. On a long resumed session `cache_read` is nearly all of it, so
/// a client that showed `input` alone would report a 400k session as holding
/// a few thousand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    /// Fresh tokens, sent and billed in full this call.
    pub input: i64,
    /// Served from the prompt cache.
    pub cache_read: i64,
    /// Written into the cache by this call.
    pub cache_creation: i64,
}

impl ContextBreakdown {
    /// What the context window sees: every input-side token, cached or not.
    ///
    /// This is the same number as `OrchestratorSessionInfo::context_tokens`,
    /// which is why that field is authoritative and this is a convenience — a
    /// client rendering the parts should not have to add them back up to draw
    /// the whole.
    pub fn total(&self) -> i64 {
        self.input + self.cache_read + self.cache_creation
    }
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
    /// A succeeded build's pull request has been open, or merged into a branch
    /// that never reached the trunk, for longer than the grace period. A pull
    /// request is not delivery any more than approval is — this is
    /// [`ObligationKind::DispatchBuild`] one stage later, and it is the only
    /// thing in the system that notices a stranded stack.
    ///
    /// **Its `subject_id` is a build id, not a spec id** — the first kind of
    /// which that is true. A batch ships or strands together, so the build is
    /// the honest unit; anything constructing a `SpecId` from `subject_id`
    /// must branch on the kind first.
    LandBatch,
    /// A decision recorded its intent, reached for somebody else's system, and
    /// never learned what happened — so a real artifact may exist upstream
    /// that the ledger does not account for.
    ///
    /// **Its `subject_id` is a decision `seq`** — the third subject type,
    /// after spec ids and build ids. Anything constructing a `SpecId` from
    /// `subject_id` must branch on the kind first; a `SpecId::from_raw("417")`
    /// is not a type error, it just heads a section with a spec that has never
    /// existed.
    ///
    /// It passes the test [`ObligationKind::StaleImage`] would have failed —
    /// it is dischargeable by its recipient — and only because the *server*
    /// holds the lookup: `GET /decisions/{seq}/reconcile` asks GitHub, with
    /// the server's own token, whether the artifact exists, and
    /// `POST /decisions/{seq}/settle` writes the answer down. An orchestrator
    /// holding a curl-only token and no `GITHUB_TOKEN` could not have
    /// discharged this honestly, and a guess written into an append-only
    /// ledger is worse than the missing row this whole change exists to
    /// prevent.
    ReconcileDecision,
}

impl ObligationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObligationKind::ReviewSpec => "review_spec",
            ObligationKind::DispatchBuild => "dispatch_build",
            ObligationKind::UnblockSpec => "unblock_spec",
            ObligationKind::LandBatch => "land_batch",
            ObligationKind::ReconcileDecision => "reconcile_decision",
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
    /// The server itself, acting on a fact it observed rather than a judgment
    /// it made — the poller closing an issue because the PR that implements it
    /// merged.
    ///
    /// Deliberately not `Human`: a write the server misattributes to the human
    /// does not fail closed, it *escalates*, because the human is never gated.
    /// Never resolved from the actor header either — only in-process code can
    /// write it.
    System,
}

impl Actor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Actor::Human => "human",
            Actor::Orchestrator => "orchestrator",
            Actor::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "human" => Some(Actor::Human),
            "orchestrator" => Some(Actor::Orchestrator),
            "system" => Some(Actor::System),
            _ => None,
        }
    }
}

/// An instruction addressed to the agent that will carry out a run, and who
/// wrote it.
///
/// **Deliberately not a `rationale`.** The two have different audiences and
/// different fates: a rationale explains a judgment to whoever reads the
/// `decisions` ledger afterwards, and nothing ever shows it to a VM;
/// directions are written *to* the agent, reach it as their own labelled
/// prompt section, and change what it does. Put an instruction in a rationale
/// and the agent never sees it; put an explanation in directions and the agent
/// acts on it. Neither is ever copied into the other.
///
/// The author travels with the text because the Builder has to be able to see
/// that what it is reading is not a Scout — see the barrier argument on
/// `builder::render_prompt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directions {
    pub text: String,
    pub author: Actor,
}

impl Directions {
    pub fn new(text: impl Into<String>, author: Actor) -> Self {
        Self {
            text: text.into(),
            author,
        }
    }

    /// Rebuild from the two nullable columns that persist this.
    ///
    /// **The text decides.** An author with no text is nothing at all, and an
    /// *unrecognized* author decays to [`Actor::Human`] rather than dropping
    /// the text — losing an instruction an agent was supposed to follow is the
    /// worse failure, and `Human` is the same default `Actor` takes for
    /// anything that did not prove otherwise.
    pub fn from_columns(text: Option<String>, author: Option<&str>) -> Option<Self> {
        let text = text?;
        Some(Self {
            text,
            author: author.and_then(Actor::from_str).unwrap_or_default(),
        })
    }

    /// The clause a prompt introduces the author with. Whole sentences would
    /// belong to whichever prompt is rendering; this is the subject alone, so
    /// the Scout's and the Builder's sections can say different things about
    /// the same author.
    pub fn author_phrase(&self) -> &'static str {
        match self.author {
            Actor::Human => "The human running this pipeline",
            Actor::Orchestrator => "The orchestrator agent",
            Actor::System => "The server",
        }
    }
}

/// Define [`DecisionAction`] and everything that must stay in step with it.
///
/// A macro rather than four hand-written matches, and the reason is a
/// property rather than tidiness: `DecisionAction::ALL` is complete **by
/// construction**. A hand-maintained array is green on the day somebody adds
/// the tenth GitHub-writing route, which is exactly the day it needed to be
/// red — `crates/tasks/tests/custodial.rs`'s
/// `no_write_route_reaches_github_without_recording_first` drives `ALL`
/// through an exhaustive match, so a new variant does not compile until
/// somebody says whether it reaches GitHub, and then the guard drives it
/// without anyone remembering to add it.
///
/// The third column is the charter capability the action belongs to. `None`
/// means no capability stands behind it, which is to say it is the human's
/// alone (`author_spec` is the only one — see `POST /tasks/{id}/build-now`).
macro_rules! decision_actions {
    ($( $(#[$meta:meta])* $variant:ident => $wire:literal, $capability:expr );* $(;)?) => {
        /// What a decision did. Narrower than [`SpecQueueStatus`] on purpose:
        /// only transitions someone *chose* are decisions, so `built`
        /// (assigned by a successful Builder run) is not one.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum DecisionAction {
            $( $(#[$meta])* $variant, )*
        }

        impl DecisionAction {
            /// Every action there is, in declaration order. Complete because
            /// the macro writes it — see the note above.
            pub const ALL: &'static [DecisionAction] = &[ $( DecisionAction::$variant, )* ];

            pub fn as_str(&self) -> &'static str {
                match self { $( DecisionAction::$variant => $wire, )* }
            }

            pub fn from_str(s: &str) -> Option<Self> {
                match s {
                    $( $wire => Some(DecisionAction::$variant), )*
                    _ => None,
                }
            }

            /// The charter capability that governs this action, if any.
            ///
            /// Read by `POST /decisions/{seq}/settle` and by the brief, so a
            /// pending row can say which capability it came from — including
            /// when that capability has since been demoted, which is the case
            /// a settle must not be refused for. `None` is human-only work.
            pub fn capability(&self) -> Option<Capability> {
                match self { $( DecisionAction::$variant => $capability, )* }
            }
        }
    };
}

decision_actions! {
    Approve => "approve", Some(Capability::AutoReviewSpecs);
    NeedsRevision => "needs_revision", Some(Capability::AutoReviewSpecs);
    Reject => "reject", Some(Capability::AutoReviewSpecs);
    RequestBuild => "request_build", Some(Capability::DispatchBuilds);
    /// A human wrote a spec by hand for a task too simple to scout, and it was
    /// approved in the same act.
    ///
    /// Deliberately not an `Approve`. An approval claims a second opinion was
    /// rendered on somebody else's artifact; here there is only one opinion in
    /// the whole loop, and one ledger row should say so rather than let a
    /// later reader mistake it for a review that happened.
    ///
    /// No capability: `POST /tasks/{id}/build-now` refuses the orchestrator
    /// outright rather than being charter-gated.
    AuthorSpec => "author_spec", None;
    /// A task was moved from the backlog into the queue, where a Scout will
    /// pick it up. Recorded because it is where spend begins.
    QueueTask => "queue_task", Some(Capability::QueueTasks);
    /// An issue was filed — work that would otherwise have been lost.
    CaptureWork => "capture_work", Some(Capability::CaptureWork);
    /// An issue was closed: finished, or judged no longer worth doing.
    RetireWork => "retire_work", Some(Capability::RetireWork);
    /// A closed issue was reopened — the recourse for a retirement that was
    /// wrong, and the reason `retire_work` can be trusted with `live`.
    ReopenWork => "reopen_work", Some(Capability::RetireWork);
    /// Something was said on an issue or a pull request. The lightest write
    /// here: recorded because a comment is still the system speaking in
    /// public under the owner's name.
    CommentOnWork => "comment_on_work", Some(Capability::CommentOnWork);
    /// A pull request was merged. The only action whose recourse is a revert
    /// rather than an edit.
    MergeBuild => "merge_build", Some(Capability::LandBuilds);
    /// A pull request was closed unmerged — the branch is not going to land.
    AbandonBuild => "abandon_build", Some(Capability::LandBuilds);
    /// A pull request was pointed at a different base branch — ordinarily the
    /// trunk, once the base it was stacked on has already reached it.
    ///
    /// Under `land_builds` with `MergeBuild` rather than its own capability,
    /// because it is the same judgment about the same artifact and the merge is
    /// what it exists to make possible. It is also the reversible one: calling
    /// it again points the pull request somewhere else, where a merge cannot be
    /// unmerged and a merged pull request can never be retargeted at all.
    RetargetBuild => "retarget_build", Some(Capability::LandBuilds);
    /// A comment pinned to a line of a pull request's diff. Separate from
    /// `CommentOnWork` because it points at code rather than at the thread,
    /// and because it can be wrong about a line that no longer exists.
    ReviewComment => "review_comment", Some(Capability::CommentOnWork);
    /// An issue's body was rewritten. The only action here that destroys
    /// rather than appends, which is why its ledger row carries the previous
    /// text: the thing worth auditing is the diff, not the event.
    EditIssue => "edit_issue", Some(Capability::CurateWork);
    /// An issue's labels were replaced.
    LabelIssue => "label_issue", Some(Capability::CurateWork);
    /// A scout or a build that was already in flight was stopped.
    ///
    /// Its own action rather than a flavour of `RequestBuild`, because the two
    /// answer different questions: one says work was started, this one says
    /// somebody decided it should not finish. The rationale on this row is the
    /// only thing that distinguishes a deliberate stop from a crash when the
    /// run is read back later.
    CancelRun => "cancel_run", Some(Capability::CancelRuns);
    /// A pending decision was reconciled against the world: the effect either
    /// happened or it did not, and the ledger now says which.
    ///
    /// Its own action rather than a rewrite of the row it settles, because
    /// `decisions` is append-only in the half that matters — who decided what
    /// and why — and a reconciliation is a *second* judgment, by a possibly
    /// different actor, about a first one. Not charter-gated: see
    /// `POST /decisions/{seq}/settle`.
    SettleDecision => "settle_decision", None;
    /// An enrollment code was minted: an external agent may now message the
    /// orchestrator under the enrolled name until the code expires. Recorded
    /// because it shapes who has a voice in the conversation — the rationale
    /// is where "which agent, and why" lives.
    EnrollAgent => "enroll_agent", Some(Capability::EnrollAgents);
    /// An enrollment was revoked before its expiry — the recourse for a mint
    /// that was wrong, and the reason `enroll_agents` can be trusted with
    /// `live`.
    RevokeAgent => "revoke_agent", Some(Capability::EnrollAgents);
}

/// What became of the effect a decision authorized.
///
/// The window this represents is the one #964 is about: every write that lands
/// in somebody else's system ran the effect and *then* recorded it, so a
/// SQLite error, a panic or a SIGKILL in between left a real artifact upstream
/// that nothing in the ledger accounts for. Recording *first* is refused —
/// a row claiming an effect a failed call never had makes every row suspect,
/// where a missing row leaves one artifact unexplained — so the window is
/// represented instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    /// The intent is on record and nobody knows what became of it. Written
    /// before the effect, and *left* there when GitHub never answered at all
    /// — which is the honest description, and what
    /// `ObligationKind::ReconcileDecision` chases.
    Pending,
    /// The effect happened. Every historical row reads as this, and so does
    /// every decision whose effect is local (a review verdict, a queueing, a
    /// cancel): those commit in the same transaction as the state they
    /// authorize, so there is no window to represent.
    Applied,
    /// The effect did not happen, and we know because the other system
    /// *answered* — a 4xx, a 429, a response we could not read. Nothing
    /// reached the world, so the row is not a history of one.
    Annulled,
}

impl DecisionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionState::Pending => "pending",
            DecisionState::Applied => "applied",
            DecisionState::Annulled => "annulled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(DecisionState::Pending),
            "applied" => Some(DecisionState::Applied),
            "annulled" => Some(DecisionState::Annulled),
            _ => None,
        }
    }

    /// Whether this is somewhere a `pending` row may move to. `pending` is
    /// not: settling is what ends the window, and a settle that leaves it open
    /// is a no-op with a ledger row.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, DecisionState::Pending)
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
    /// Stop a scout or a build that is already running.
    ///
    /// Not folded into `dispatch_builds`: starting work and stopping it have
    /// unrelated failure modes — one spends a VM hour, the other throws one
    /// away — and a human who wants one without the other has to be able to
    /// say so.
    CancelRuns,
    /// Mint (and revoke) short-lived enrollment codes that let an external
    /// agent send messages into the orchestrator conversation under its own
    /// name.
    ///
    /// A capability rather than human-only, because the convenient flow is
    /// asking the orchestrator in chat for a code and pasting its reply into
    /// the other agent — no app UI in the loop. What makes that safe to
    /// grant: an enrollment conveys a *voice*, not authority (an agent turn
    /// is input the orchestrator weighs, never a gated write), every mint is
    /// a ledger row with a rationale, and the code names its holder — so the
    /// recourse for a bad mint is `POST /agents/{id}/revoke`, which this same
    /// capability covers for the same reason `reopen_work` lives under
    /// `retire_work`: an act and its undo belong to one switch.
    EnrollAgents,
}

impl Capability {
    /// Every capability, in the order the charter is meant to be flipped:
    /// additive and trivially reversible first, irreversible-ish last.
    pub const ALL: [Capability; 10] = [
        Capability::CaptureWork,
        Capability::CommentOnWork,
        Capability::RetireWork,
        Capability::QueueTasks,
        Capability::DispatchBuilds,
        Capability::CancelRuns,
        Capability::AutoReviewSpecs,
        Capability::LandBuilds,
        Capability::CurateWork,
        Capability::EnrollAgents,
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
            Capability::CancelRuns => "cancel_runs",
            Capability::EnrollAgents => "enroll_agents",
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
            "cancel_runs" => Some(Capability::CancelRuns),
            "enroll_agents" => Some(Capability::EnrollAgents),
            _ => None,
        }
    }

    /// Every capability again, ordered by what it does to somebody's
    /// repository — the sharpest first.
    ///
    /// A **second ordering**, not a re-sort of [`Self::ALL`] and not a
    /// reversal of it. `ALL` is the order the charter is meant to be *flipped*
    /// in, additive first and irreversible last, which puts `CurateWork` at
    /// the tail — and reading that backwards would put "revise an issue you
    /// filed" at the head of a list a person reads to find out what is about
    /// to happen to their repository. Merging pull requests belongs there.
    ///
    /// Length is `ALL.len()`, so a tenth capability fails to compile here
    /// rather than going quietly missing from whatever renders this.
    pub const BY_CONSEQUENCE: [Capability; Capability::ALL.len()] = [
        Capability::LandBuilds,
        Capability::RetireWork,
        Capability::CurateWork,
        Capability::CaptureWork,
        Capability::CommentOnWork,
        Capability::DispatchBuilds,
        Capability::QueueTasks,
        Capability::AutoReviewSpecs,
        Capability::CancelRuns,
        Capability::EnrollAgents,
    ];

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
            Capability::CancelRuns => "stop a scout or a build that is already running",
            Capability::EnrollAgents => {
                "enroll an external agent: mint a short-lived code that lets it \
                 message you (POST /agents), or revoke one"
            }
        }
    }

    /// The same fact as [`Self::describe`], said to the person who owns the
    /// repository rather than to the agent holding the permission.
    ///
    /// **Arms of a second exhaustive match over the same enum**, deliberately:
    /// a tenth capability cannot answer one of these and not the other,
    /// because it fails to compile. That is the whole guarantee, and it is why
    /// this lives here rather than in whichever client renders it — a ninth
    /// line in a client's table simply goes missing.
    ///
    /// `describe` says "file issues for work you discover", which is a job
    /// description and reads as harmless; the person deciding whether to let
    /// this loose is asking "what appears on my repository", and the honest
    /// answer is "new issues, filed without asking". Same enforced row, same
    /// structure, different audience.
    pub fn consequence(&self) -> &'static str {
        match self {
            Capability::CaptureWork => "file new issues on your repositories",
            Capability::RetireWork => "close your issues",
            Capability::QueueTasks => "put work in the queue for an agent to pick up",
            Capability::DispatchBuilds => "start builds that write code and open pull requests",
            Capability::AutoReviewSpecs => "approve its own plans without you reading them",
            Capability::CommentOnWork => "comment on your issues and pull requests",
            Capability::LandBuilds => {
                "merge its own pull requests into your default branch, or close them unmerged"
            }
            Capability::CurateWork => "rewrite the body and labels of issues it filed",
            Capability::CancelRuns => "stop a scout or a build that is already running",
            Capability::EnrollAgents => {
                "hand another agent on this machine a short-lived code that lets it \
                 speak into the orchestrator's conversation"
            }
        }
    }

    /// Changes something outside Tasks that the person cannot take back.
    ///
    /// **This means "cannot be undone", not "worth being told about"**, and
    /// the difference matters when reading the `false` arms. Filing thirty
    /// issues under `CaptureWork`, or commenting under `CommentOnWork`, is
    /// among the most *visible* things this system can do to a public
    /// repository — and both are deletable, so both are quiet here. Their
    /// absence is a statement about reversibility and not a judgement that
    /// nobody would mind. What carries the weight for a reader is the
    /// unconditional list of what Play does at all; this only decides which
    /// capabilities get named in the headline sentence.
    ///
    /// Exhaustive, so a new capability is classified rather than defaulting
    /// into the quiet half.
    pub fn is_sharp(&self) -> bool {
        match self {
            // Lands somebody else's code in a branch people build on, or
            // closes a pull request the work is sitting in.
            Capability::LandBuilds => true,
            // Closing an issue upstream is a state change on the repository
            // that the person did not make.
            Capability::RetireWork => true,
            // Rewrites rather than appends: a bad capture leaves a bad issue,
            // a bad edit destroys a good one.
            Capability::CurateWork => true,
            // Reversible: delete the issue, delete the comment.
            Capability::CaptureWork => false,
            Capability::CommentOnWork => false,
            // Local to the pipeline. They spend machine time and API credit —
            // which the notice states unconditionally — but nothing outside
            // Tasks changes, and nothing here is unrecoverable.
            Capability::QueueTasks => false,
            Capability::DispatchBuilds => false,
            Capability::AutoReviewSpecs => false,
            // Throws away a VM hour rather than making one.
            Capability::CancelRuns => false,
            // Conveys a voice and not authority: an enrolled agent's turn is
            // input the orchestrator weighs, never a gated write. The code
            // expires on its own and can be revoked before it does.
            Capability::EnrollAgents => false,
        }
    }

    /// The name a person reads. The slug stays the API's.
    pub fn title(&self) -> &'static str {
        match self {
            Capability::CaptureWork => "File issues",
            Capability::RetireWork => "Close issues",
            Capability::QueueTasks => "Queue tasks",
            Capability::DispatchBuilds => "Start builds",
            Capability::AutoReviewSpecs => "Review specs",
            Capability::CommentOnWork => "Comment",
            Capability::LandBuilds => "Merge pull requests",
            Capability::CurateWork => "Edit issues",
            Capability::CancelRuns => "Cancel runs",
            Capability::EnrollAgents => "Enroll agents",
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
    /// What became of the effect. See [`DecisionState`].
    ///
    /// One row with a state column, and deliberately not an intent row plus a
    /// confirmation row: every existing aggregate over `decisions` would
    /// double-count under two rows — the daily cap, `has_decision`, and the
    /// `NOT EXISTS` behind the `ReviewSpec` obligation — and each would need
    /// an "and not the intent one" clause the next query written against this
    /// table would forget. One row keeps every reader correct without being
    /// taught anything.
    #[serde(default = "crate::models::default_applied")]
    pub state: DecisionState,
    /// What the effect produced or refused with — an issue number, a merge
    /// SHA, GitHub's own error. Merged rather than replaced when a row is
    /// settled, so the error a refused call wrote here survives the
    /// reconciliation that later finds the artifact.
    ///
    /// The mutable half of the row. Actor, action, rationale, evidence and
    /// subject are never rewritten.
    #[serde(default)]
    pub outcome: Option<serde_json::Value>,
    /// When the row left `pending`. `None` while it is still open, and `None`
    /// on every row that was never pending at all.
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub(crate) fn default_applied() -> DecisionState {
    DecisionState::Applied
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
    /// A tick began. Published before the agent is spawned, so a client can
    /// show that work is under way during the long silence before the first
    /// token — and so a *proactive* tick (one no client asked for) is visible
    /// at all. Payload-free on purpose: a client times the wait it actually
    /// witnessed rather than inventing history it missed.
    Started,
    /// A chunk of assistant text, in generation order.
    Delta { text: String },
    /// The agent invoked a tool (e.g. a curl against this API).
    Tool { label: String },
    /// The tick finished; fetch `/orchestrator/messages` for the reply.
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pipeline state, in declaration order. Kept honest by
    /// [`all_states_lists_every_variant`]: a new variant is a compile error in
    /// that test's match, and a runtime failure until it is listed here too.
    const ALL_STATES: [TaskState; 9] = [
        TaskState::Backlog,
        TaskState::Queued,
        TaskState::Scouting,
        TaskState::InReview,
        TaskState::ReadyToBuild,
        TaskState::Building,
        TaskState::AwaitingMerge,
        TaskState::Done,
        TaskState::Rejected,
    ];

    #[test]
    fn all_states_lists_every_variant() {
        // The match is exhaustive on purpose: adding a variant stops this
        // compiling, and the assertion then stops it passing until the array
        // above grows too.
        for state in ALL_STATES {
            let listed = match state {
                TaskState::Backlog
                | TaskState::Queued
                | TaskState::Scouting
                | TaskState::InReview
                | TaskState::ReadyToBuild
                | TaskState::Building
                | TaskState::AwaitingMerge
                | TaskState::Done
                | TaskState::Rejected => ALL_STATES.contains(&state),
            };
            assert!(listed, "{} is missing from ALL_STATES", state.as_str());
            assert_eq!(TaskState::from_str(state.as_str()), Some(state));
        }
    }

    /// SQLite cannot call [`TaskState::is_terminal`], so the clause spells the
    /// terminal states out. This is the seam that notices when the two drift.
    #[test]
    fn the_terminal_sort_clause_names_exactly_the_terminal_states() {
        for state in ALL_STATES {
            let named =
                TaskState::ORDER_TERMINAL_LAST_SQL.contains(&format!("'{}'", state.as_str()));
            assert_eq!(
                named,
                state.is_terminal(),
                "{} is named in ORDER_TERMINAL_LAST_SQL but is_terminal() disagrees",
                state.as_str()
            );
        }
    }

    /// `awaiting_merge` is live work: a PR that opened is a claim, not a
    /// delivery, and the poller is still driving it.
    #[test]
    fn awaiting_merge_is_not_terminal() {
        assert!(!TaskState::AwaitingMerge.is_terminal());
        assert!(TaskState::Done.is_terminal());
        assert!(TaskState::Rejected.is_terminal());
    }

    /// A second ordering, not a second *set*. The compiler pins the length;
    /// this pins that nothing was dropped or doubled while reordering, which
    /// a fixed-length array cannot catch on its own.
    #[test]
    fn by_consequence_is_a_permutation_of_all() {
        let mut ordered = Capability::BY_CONSEQUENCE.to_vec();
        let mut all = Capability::ALL.to_vec();
        ordered.sort_by_key(|c| c.as_str());
        all.sort_by_key(|c| c.as_str());
        assert_eq!(ordered, all);
    }

    /// ...and it is genuinely a different order, not `ALL` copied. If someone
    /// "tidies" it into a reversal of `ALL`, `curate_work` leads a list a
    /// person reads to find out what is about to happen to their repository.
    #[test]
    fn by_consequence_leads_with_merging_rather_than_editing() {
        assert_eq!(Capability::BY_CONSEQUENCE[0], Capability::LandBuilds);
        assert_ne!(Capability::BY_CONSEQUENCE, Capability::ALL);
        let reversed: Vec<_> = Capability::ALL.iter().rev().copied().collect();
        assert_ne!(Capability::BY_CONSEQUENCE.to_vec(), reversed);
    }

    /// Every rendering is an arm of an exhaustive match, so this cannot fail
    /// by omission — only by someone writing an empty string into one.
    #[test]
    fn every_capability_says_something_in_every_voice() {
        for capability in Capability::ALL {
            assert!(!capability.describe().trim().is_empty());
            assert!(!capability.consequence().trim().is_empty());
            assert!(!capability.title().trim().is_empty());
        }
    }

    /// The two voices are the same fact, not the same sentence: `describe`
    /// addresses the agent, `consequence` addresses the repository's owner.
    /// If they ever collapse into one string the distinction has been lost by
    /// edit rather than by decision.
    #[test]
    fn the_two_voices_are_not_the_same_words() {
        let differing = Capability::ALL
            .iter()
            .filter(|c| c.describe() != c.consequence())
            .count();
        assert_eq!(differing, Capability::ALL.len() - 1);
        // The one that legitimately coincides: stopping a run is the same act
        // described to either audience, and inventing a difference would be
        // worse than sharing the sentence.
        assert_eq!(
            Capability::CancelRuns.describe(),
            Capability::CancelRuns.consequence()
        );
    }

    /// Sharpness is about reversibility. Pinned per capability rather than by
    /// counting, so a reclassification is a deliberate edit to this list.
    #[test]
    fn sharp_means_it_cannot_be_taken_back() {
        for capability in [
            Capability::LandBuilds,
            Capability::RetireWork,
            Capability::CurateWork,
        ] {
            assert!(capability.is_sharp(), "{}", capability.as_str());
        }
        for capability in [
            Capability::CaptureWork,
            Capability::CommentOnWork,
            Capability::QueueTasks,
            Capability::DispatchBuilds,
            Capability::AutoReviewSpecs,
            Capability::CancelRuns,
        ] {
            assert!(!capability.is_sharp(), "{}", capability.as_str());
        }
    }
}
