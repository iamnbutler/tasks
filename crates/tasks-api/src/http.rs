//! Request and response bodies that aren't themselves domain types.
//!
//! Each shape derives both `Serialize` and `Deserialize`: the server
//! deserializes requests and serializes responses, a client does the
//! reverse, and both use these definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{BriefingSection, Build, Complexity, Mode, ProjectId, SpecId, TaskId};

/// Body of `POST /projects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateProject {
    pub repo_owner: String,
    pub repo_name: String,
}

/// The answer to a write the charter shadowed: the decision was recorded and
/// nothing happened. Deliberately not the normal success body — a shadow run
/// whose responses look like real ones is an evaluation that lies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowAck {
    /// Always `true`; present so a caller can branch on one field.
    pub shadowed: bool,
    /// The ledger row holding the judgment that was not applied.
    pub decision_seq: i64,
    /// What did not happen, in words, for the agent reading the response.
    pub note: String,
}

/// Body of `POST /charter/{capability}` — set what the orchestrator may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetCharter {
    /// `off`, `shadow`, or `live`.
    pub level: String,
    /// Optional manual brake: actions per day. `null` (the default) is
    /// uncapped, which is what every capability ships as.
    #[serde(default)]
    pub daily_limit: Option<i64>,
}

/// Body of `POST /issues` — file an issue upstream and track it here.
///
/// The write happens on the server rather than through an agent's own `gh`
/// credential, which is what makes it show up in the ledger, in the event log,
/// and under whatever caps the charter sets. An agent filing issues out of band
/// is not a smaller version of this; it is the ungoverned version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureIssue {
    /// Which repository. Optional when exactly one project is configured —
    /// the common case, and one less thing for a caller to get wrong.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Where this came from — "discovered while reviewing spec_… for #812".
    /// Required of the orchestrator and rendered into the issue body, so the
    /// capture is auditable from GitHub alone and a human can judge whether
    /// the instinct is set too loose.
    #[serde(default)]
    pub provenance: Option<String>,
    /// Why this is worth filing. Required of the orchestrator.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /tasks/{task_id}/close` — retire work upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseTaskRequest {
    /// `completed` or `not_planned`. Not cosmetic: `completed` claims the work
    /// was done and wants evidence to match, `not_planned` is a recalibration.
    pub reason: String,
    /// Why. Required of the orchestrator — "no longer relevant" is the one
    /// custodial call with no cheap evidence standard, so the reasoning is the
    /// only thing a human can review it by.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What was checked: the merged PR, the commit that did it. Free-form
    /// JSON, stored verbatim on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /tasks/{task_id}/reopen` — undo a retirement.
///
/// The recourse half of [`CloseTaskRequest`]. A capability that can close work
/// and cannot reopen it makes every mistaken close permanent for whoever finds
/// it next, which is a strange shape for a system whose whole safety argument
/// is "audit and recourse, not pre-approval".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReopenTaskRequest {
    /// What changed. Reopening contradicts an earlier recorded decision, so
    /// the ledger should say why the earlier one no longer holds.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /issues/{number}/comments` — say something on an issue or a
/// pull request.
///
/// One route for both because GitHub's comment endpoint makes no distinction:
/// a PR *is* an issue as far as `/issues/{n}/comments` is concerned, and they
/// share one number space. Splitting it into two routes here would invent a
/// difference the API does not have.
///
/// This exists because a verdict with nowhere to go is a verdict that comes
/// back as prose for a human to re-read and re-type. That is the same waste
/// shadow mode produced, arriving by a different road.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentRequest {
    /// Which repository. Optional when exactly one project is configured.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// The comment, as markdown.
    pub body: String,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /pull-requests/{number}/merge`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergePullRequest {
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// `merge`, `squash`, or `rebase`. Defaults to `squash`, which is what
    /// every Builder PR in this repo has been merged with.
    #[serde(default)]
    pub method: Option<String>,
    /// The commit subject. Defaults to GitHub's own, which is the PR title.
    #[serde(default)]
    pub commit_title: Option<String>,
    /// Why this is safe to land. Required of the orchestrator: merging is the
    /// one write here whose recourse is a revert rather than an edit, so the
    /// ledger row has to be worth reading on its own.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What was checked — CI conclusion, the review, the diff. Free-form JSON.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /pull-requests/{number}/close` — close a PR without merging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbandonPullRequest {
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// Why this branch is not going to land. Required of the orchestrator —
    /// abandoning a Builder run discards work that cost a VM hour, and the
    /// only thing that makes that reviewable is the stated reason.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /queue/reorder`: the complete queue order, front to back.
/// Tasks not listed are left unranked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReorderQueue {
    pub task_ids: Vec<TaskId>,
}

/// Body of `POST /spec-queue/reorder`. Same semantics as [`ReorderQueue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReorderSpecQueue {
    pub spec_ids: Vec<SpecId>,
}

/// Body of `POST /spec-queue/{spec_id}/review`. `status` is a string rather
/// than a [`crate::models::SpecQueueStatus`] so the server can answer an
/// unknown value with a 400 of its own instead of a deserialization
/// rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub status: String,
    /// Sent onward to a scout on `needs_revision` — what to change.
    #[serde(default)]
    pub feedback: Option<String>,
    /// Why this verdict was rendered. Distinct from `feedback`: that is
    /// addressed to the scout, this is the decision record. Required when
    /// the caller is the orchestrator — an autonomous verdict with no stated
    /// reason is not auditable, so the server refuses it.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What the decider checked, free-form JSON (staleness, overlaps,
    /// verification claims). Stored verbatim on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /builds`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildRequest {
    pub spec_ids: Vec<SpecId>,
    /// Branch the batch is cut from and PR'd against. Defaults to `main`.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Why this batch, now. Required of the orchestrator — batching and
    /// ordering are judgment calls, so the reasoning is the record.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What the decider checked, free-form JSON. Stored on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /tasks/{task_id}/build-now`.
///
/// Every field is optional, because the common case — "the issue body already
/// *is* the spec" — is an empty body. Human-only: the orchestrator gets a 403
/// whatever its charter says, since authoring and approving its own work with
/// no second opinion anywhere in the loop is a different autonomy from
/// `dispatch_builds`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildNowRequest {
    /// The spec text. **Replaces** the issue body rather than adding to it —
    /// the Builder prompt is spec content alone, so anything supplied here is
    /// everything the Builder will see. Omit it to use the issue body.
    #[serde(default)]
    pub content: Option<String>,
    /// How big the work is. Defaults to `simple`: a task worth skipping the
    /// Scout for is one whose shape is already known.
    #[serde(default)]
    pub complexity: Option<Complexity>,
    /// Branch the build is cut from and PR'd against. Defaults to `main`.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Why this task did not need scouting. Not required — the human is never
    /// gated — but it is the only record of the judgment.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What the decider checked, free-form JSON. Stored on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// A build with its batch, in position order — the shape of
/// `GET /builds/{id}` and the `POST /builds` acknowledgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildDetail {
    #[serde(flatten)]
    pub build: Build,
    pub spec_ids: Vec<SpecId>,
}

/// Body of `POST /orchestrator/messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendMessage {
    pub content: String,
}

/// Body of `POST /mode`. `mode` is a string for the same reason as
/// [`ReviewRequest::status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetMode {
    pub mode: String,
}

/// Response of `GET /mode` and `POST /mode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeResponse {
    pub mode: Mode,
}

/// Response of `GET /status` — the liveness answer, from the process that
/// owns the database.
///
/// Half of this is knowable only from the running process (its pid, when
/// *this* boot started, which migrations *this* boot applied) and half only
/// from the store (mode, work in flight). It is one route because a
/// supervisor needs both in one answer: a 200 here is the claim "this binary
/// opened the database, ran its migrations, and is serving".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    /// The serving process. A supervisor verifies *this*, not "something
    /// answered the port" — a stale listener satisfies the latter.
    pub pid: u32,
    /// When this boot began serving.
    pub started_at: DateTime<Utc>,
    /// Migrations this boot actually applied — empty when the schema was
    /// already current. Not "migrations this binary ships with": the
    /// operator's question is whether the schema moved under them.
    pub migrations_applied: Vec<AppliedMigration>,
    pub mode: Mode,
    pub in_flight: InFlight,
}

/// One migration a boot applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedMigration {
    pub version: i64,
    /// As sqlx records it, which is the filename with underscores turned into
    /// spaces (`manual rank`). Render via [`AppliedMigration::file_stem`].
    pub description: String,
}

impl AppliedMigration {
    /// `0002_manual_rank` — the migration's filename stem, so a report is
    /// greppable against `crates/tasks/migrations/`.
    ///
    /// The `{:04}` is for the legacy sequence, whose filenames are zero-padded
    /// and would not be greppable without it. New migrations are named for a
    /// UTC instant (`20260815030411_build_transcripts`), where padding to four
    /// digits is a no-op — so one format string spans both eras.
    pub fn file_stem(&self) -> String {
        format!("{:04}_{}", self.version, self.description.replace(' ', "_"))
    }
}

/// Work a restart would interrupt, as `GET /status` reports it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InFlight {
    /// `running` scout sessions.
    pub scouts: Vec<InFlightItem>,
    /// `running` builds. Queued ones are deliberately absent: durable intent
    /// survives a restart, and counting it would make a healthy backlog read
    /// as a permanent reason never to restart.
    pub builds: Vec<InFlightItem>,
    /// An orchestrator turn the pipeline owes, if any. Reported, never a
    /// reason to wait — see [`InFlight::is_destructible`].
    pub orchestrator: Option<InFlightItem>,
}

impl InFlight {
    /// Work a restart would destroy: a scout or a build in a VM that nobody
    /// resumes. This is the drain condition.
    ///
    /// An owed orchestrator turn is not here on purpose. The obligation and
    /// nudge loops keep producing input, so waiting for the conversation to
    /// settle can wait forever; and the answered watermark only advances with
    /// the reply, so a restart mid-turn costs one agent turn and the next boot
    /// takes it again.
    pub fn is_destructible(&self) -> bool {
        !self.scouts.is_empty() || !self.builds.is_empty()
    }

    /// Nothing at all in flight, owed turns included.
    pub fn is_empty(&self) -> bool {
        !self.is_destructible() && self.orchestrator.is_none()
    }
}

/// One piece of in-flight work: what it is, and how long it has been at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlightItem {
    /// Session id, build id, or the seq of the oldest unanswered turn.
    pub id: String,
    /// What it is working on — the task behind a scout, the branch of a build.
    #[serde(default)]
    pub detail: Option<String>,
    /// When it started, so a report can age it.
    pub since: DateTime<Utc>,
}

/// One Home briefing slot as `GET /briefings` serves it. All three sections
/// are always present; a never-generated one has no content and reads as
/// stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefingStatus {
    pub section: BriefingSection,
    pub content: Option<String>,
    pub generated_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub regenerating: bool,
    /// The last generation failure, if the most recent attempt failed. The
    /// stored content (if any) is still served alongside it — never a blank
    /// slot, never a fabricated one.
    pub error: Option<String>,
}

/// Error body every non-2xx response carries: `{"error": "..."}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Body of `POST /pull-requests/{number}/review-comments` — a comment pinned
/// to one line of the diff.
///
/// The head SHA is deliberately not a field. GitHub anchors a review comment
/// to a commit, and a SHA that arrived through a prompt is precisely the kind
/// of GitHub-owned fact this system refuses to carry: the server reads the
/// current head at comment time instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCommentRequest {
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// Repo-relative path, as it appears in the diff.
    pub path: String,
    /// Line number in the file *after* the change. The file has to actually
    /// appear in the diff — GitHub refuses otherwise, which is correct: a
    /// review comment on an unchanged line is one nobody sees.
    pub line: u64,
    pub body: String,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /issues/{number}/edit` — rewrite an issue's title or body.
///
/// The only write in the API that destroys rather than appends, which is why
/// the server reads the current text first and stores it on the decision. An
/// issue built on a theory that later collapses is worse than no issue: the
/// next reader inherits the superseded reasoning as though it still held. But
/// "the orchestrator edited #835" is not an auditable record — the diff is —
/// so the ledger keeps the old text whether anyone asked for it or not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditIssueRequest {
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// Omit to leave unchanged.
    #[serde(default)]
    pub title: Option<String>,
    /// Omit to leave unchanged. Replaces the body entirely.
    #[serde(default)]
    pub body: Option<String>,
    /// What changed and why the earlier text no longer holds. Required of the
    /// orchestrator — an edit with no reason is indistinguishable from a
    /// mistake once the old text is only in the ledger.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /issues/{number}/labels` — replace an issue's labels.
///
/// The complete set, not an addition, so removing a label is expressible.
/// Read the vocabulary from `GET /labels` first: labelling from a guessed
/// vocabulary is how a repo ends up with `bug` and `bugs`, and every filter
/// written afterwards is quietly wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetLabelsRequest {
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    pub labels: Vec<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// One entry of `GET /labels`: the repository's label vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelInfo {
    pub name: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::AppliedMigration;

    fn applied(version: i64, description: &str) -> AppliedMigration {
        AppliedMigration {
            version,
            description: description.to_string(),
        }
    }

    /// Both eras of migration name reconstruct: the zero-padded legacy
    /// sequence needs the `{:04}`, and a 14-digit UTC stamp is unharmed by it.
    #[test]
    fn a_file_stem_is_greppable_in_both_naming_eras() {
        assert_eq!(applied(2, "manual rank").file_stem(), "0002_manual_rank");
        assert_eq!(
            applied(20260815030411, "build transcripts").file_stem(),
            "20260815030411_build_transcripts"
        );
    }
}
