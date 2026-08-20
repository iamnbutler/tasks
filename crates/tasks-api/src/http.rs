//! Request and response bodies that aren't themselves domain types.
//!
//! Each shape derives both `Serialize` and `Deserialize`: the server
//! deserializes requests and serializes responses, a client does the
//! reverse, and both use these definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{Build, BuildId, Mode, ProjectId, RunKind, SpecId, TaskId};
use crate::version::ImageIdentity;

/// Body of `POST /projects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateProject {
    pub repo_owner: String,
    pub repo_name: String,
}

/// Body of `POST /projects/{project_id}/status` — how much of the pipeline
/// runs for one repo.
///
/// A string rather than the enum, like [`SetCharter`]: an unknown word comes
/// back as a 400 naming the three legal ones instead of serde's "unknown
/// variant `pasued`, expected one of …".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetProjectStatus {
    /// `active`, `paused`, or `archived`.
    pub status: String,
}

/// Settle a pending decision: say what became of an effect nobody saw the
/// answer to.
///
/// `state` is `applied` or `annulled`; `pending` is a 400, because settling is
/// what ends the window and a settle that leaves it open is a no-op with a
/// ledger row behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettleDecisionRequest {
    pub state: String,
    /// Merged into the pending row's `outcome`, never replacing it — so the
    /// error a refused call left there survives the reconciliation that found
    /// the artifact.
    #[serde(default)]
    pub outcome: Option<serde_json::Value>,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// What the server found when it went looking for a pending decision's
/// artifact — `GET /decisions/{seq}/reconcile`.
///
/// The lookup lives on the **server** and not in the caller, because the
/// server holds the GitHub credential and the caller usually does not: the
/// orchestrator runs with a curl-only allowlist and no `GITHUB_TOKEN`, so an
/// obligation whose honest discharge needed its own GitHub read would leave
/// guessing as its only move — and a guess written into an append-only ledger
/// is worse than the missing row the whole intent mechanism exists to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionReconciliation {
    pub seq: i64,
    pub action: String,
    /// `applied`, `annulled`, or `unknown` — the state this row should be
    /// settled to, as far as GitHub could say. `unknown` is never a licence to
    /// guess: it means the lookup itself could not answer, and the row stays
    /// pending until it can.
    pub verdict: String,
    /// What the server actually saw, so the settle is written from evidence
    /// the server produced rather than from the caller's recollection.
    pub found: serde_json::Value,
    /// One line a human or an agent can act on.
    pub note: String,
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

/// Body of `POST /pull-requests/{number}/retarget`.
///
/// The verb a stacked pull request needs once its base has already landed
/// (#1027). Until it existed, that state was diagnosed precisely by the brief
/// and had no act behind it: merging adds a commit to a branch nothing will
/// pick up, and a merged pull request can never be retargeted afterwards — so
/// the instructed default was the irreversible one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetargetPullRequest {
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// The branch this pull request should merge into instead — ordinarily the
    /// trunk, once the base it was stacked on has reached it.
    pub base: String,
    /// Why. Required of the orchestrator like every other write here; unlike a
    /// merge, this one is reversible by calling it again.
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
    /// `merge`, `squash`, or `rebase`. Defaults to **`merge`** (#1044).
    ///
    /// Not a style preference: a merge commit leaves the head branch an
    /// ancestor of the trunk forever, and a squash does not — it writes one
    /// new commit and the branch it came from is an ancestor of nothing. This
    /// pipeline stacks builds routinely, and a dependent build's only cheap
    /// diagnosis is branch ancestry, so a squashed base leaves its dependents
    /// undiagnosable *and* unretargetable. The method is therefore a choice
    /// about whether a stranded dependent is recoverable at all.
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
    /// Addressed to whichever agent picks the work up next, and it reaches one
    /// on **either** verdict: on `needs_revision` the re-scout is asked to
    /// account for it in `SPEC.md`'s `### Notes`, and on `approved` the Builder
    /// gets it as its own `## Review feedback on these specs` section and is
    /// asked to account for it in `SUMMARY.md`. So an approval may carry
    /// required changes — this is how a reviewer approves *with* them rather
    /// than sending the whole spec back.
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
    ///
    /// Read by humans afterwards and **never sent to the Builder**. To tell
    /// the Builder something, use `directions`.
    #[serde(default)]
    pub rationale: Option<String>,
    /// Instructions for the Builder itself, carried into its prompt as their
    /// own section and stored on the build row. Not a second `rationale`:
    /// see [`crate::models::Directions`].
    #[serde(default)]
    pub directions: Option<String>,
    /// What the decider checked, free-form JSON. Stored on the decision.
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /tasks/{id}/queue` and `POST /tasks/{id}/scout`.
///
/// Every field is optional and the body itself may be omitted entirely — both
/// routes took no body at all before `directions` existed, and every caller
/// that still sends none keeps working.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ScoutRequest {
    /// Instructions for the Scout that picks this task up, staged on the task
    /// until one does.
    ///
    /// Three-way, and the distinction is the point: **absent** leaves whatever
    /// is staged alone (posting `/scout` a second time with no body must not
    /// unaim the run), **empty or whitespace** clears it, and text stages it.
    #[serde(default)]
    pub directions: Option<String>,
    /// Why this task, now. Required of the orchestrator. Lands on the decision
    /// row and is never shown to the Scout.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /tasks/{task_id}/build-now` — skip scouting for a task whose
/// issue body already is the specification.
///
/// Every field is optional, because the common case is "the issue says it
/// all": an empty body writes the issue body as the spec, approves it, and
/// queues a Builder run over it. Human-only — the server refuses the
/// orchestrator outright, since authoring and approving one's own spec with no
/// second opinion anywhere in the loop is a different kind of autonomy from
/// anything in the charter today.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BuildNowRequest {
    /// The spec, if the issue body is not it. **Replaces** the body rather
    /// than extending it — the Builder prompt is spec content alone, so a
    /// supplied `content` is the whole of what the Builder will read.
    #[serde(default)]
    pub content: Option<String>,
    /// `simple` \| `medium` \| `complex`. Defaults to `simple`: a task worth
    /// skipping the Scout for is one nobody needed to explore.
    #[serde(default)]
    pub complexity: Option<String>,
    /// Branch the build is cut from and PR'd against. Defaults to `main`.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Why this needed no spec. Not required — only the orchestrator owes
    /// explanations, and the orchestrator cannot call this at all — but it is
    /// the one thing that makes an unreviewed build reviewable afterwards.
    #[serde(default)]
    pub rationale: Option<String>,
    /// Instructions for the Builder, kept strictly out of the spec this
    /// authors. `content` is the specification; `directions` is what to do
    /// with it, and conflating them would put an instruction to the agent into
    /// the artifact a reviewer reads.
    #[serde(default)]
    pub directions: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /sessions/{id}/cancel` and `POST /builds/{id}/cancel` — stop
/// work that is already in flight.
///
/// Both routes take the same body because a cancel says the same thing about
/// either: who asked, and why. Everything else the server already knows from
/// the row.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CancelRunRequest {
    /// Why this run should not finish. Required of the orchestrator, and the
    /// text that lands in the run's `exit_reason` — which is what lets a
    /// reader tell a deliberate stop from a crash long afterwards.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// The answer to a cancel: what was asked, and whether it has landed yet.
///
/// `concluded` is the load-bearing field. A cancel is a request the dispatcher
/// following the run has to notice, so the server polls briefly and then
/// answers honestly rather than claiming success it has not observed. `false`
/// is not a failure — it means "asked, not yet concluded", and the run's
/// terminal event is still coming.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelAck {
    pub run_kind: RunKind,
    pub run_id: String,
    /// Whether the run had reached a terminal state by the time this answered.
    pub concluded: bool,
    /// The run's status as of this answer — terminal when `concluded`.
    pub status: String,
    /// The ledger row holding the decision to stop it.
    pub decision_seq: i64,
    /// What happened, in words, for whoever (or whatever) is reading.
    pub note: String,
}

/// The answer to `POST /runs/cancel-all`: one [`CancelAck`] per run that was
/// asked to stop, and a one-line summary.
///
/// An empty `runs` with a note is a real answer, not a failure — either
/// nothing was running, or the capability is shadowed and the decisions were
/// recorded without being applied. The note says which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelAllResponse {
    pub runs: Vec<CancelAck>,
    pub note: String,
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

/// Body of `POST /agents` — mint an enrollment code for an external agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollAgentRequest {
    /// The name the agent's messages will carry, chosen by whoever mints —
    /// never by the agent, because the name is what the orchestrator (and the
    /// human reading the transcript) attributes the words to.
    pub name: String,
    /// How long the code lives. Bounded, and an out-of-range value is a 400
    /// rather than a clamp — a credential silently granted a different
    /// lifetime than the one asked for is a different grant.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    /// Why this agent, now. Required of the orchestrator; lands on the
    /// decision row.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Response of `POST /agents`. The one and only time the code is visible —
/// the server keeps a hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollAgentResponse {
    #[serde(flatten)]
    pub enrollment: crate::models::AgentEnrollment,
    /// The bearer code, shown once. Hand it to the agent; it goes in the
    /// `X-Tasks-Agent` header of `POST /orchestrator/messages`.
    pub code: String,
}

/// Body of `POST /agents/{id}/revoke`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RevokeAgentRequest {
    /// Required of the orchestrator; lands on the decision row.
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub evidence: Option<serde_json::Value>,
}

/// Body of `POST /mode`. `mode` is a string for the same reason as
/// [`ReviewRequest::status`].
///
/// `note` is why this mode was set, for the event feed. It exists because the
/// artifact a maintenance drain leaves behind is a `pause` byte-identical to
/// any other: an hour later `/status`, `tasks status` and the Server window
/// all say `pause` and nothing says why. A caller that has a reason sends it
/// and the server appends it as an [`crate::events::EventPayload::Note`] —
/// the edge is on the feed, the standing answer is the mode, and there is
/// deliberately nothing between them (a persisted "held for maintenance"
/// would be a fourth hold to keep in step). `#[serde(default)]`, so every
/// existing caller keeps working and sending none is the same request it
/// always was.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SetMode {
    pub mode: String,
    #[serde(default)]
    pub note: Option<String>,
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
    /// What the VM images are running, as last observed by a run that started
    /// inside one, judged against *this* server's build.
    ///
    /// `#[serde(default)]` is **required, not decorative**: `reload`'s
    /// `fetch_status` reads `/status` off the *old*, still-running server
    /// before it swaps, so the binary decoding this is by construction newer
    /// than the one answering it. A required field would make every upgrade
    /// past this commit fail at exactly the step whose job is to verify the
    /// upgrade.
    ///
    /// An empty list means nothing has been observed — which is **not** a
    /// clean bill of health, and every renderer says so rather than printing
    /// "current".
    #[serde(default)]
    pub images: Vec<ImageIdentity>,
    /// Set for as long as scout and build dispatch is being held because GitHub
    /// is not answering; `None` the rest of the time, which is almost always.
    ///
    /// `#[serde(default)]` for the same reload-skew reason as `images`:
    /// `reload` reads `/status` off the *older* server before it swaps, so the
    /// binary decoding this is by construction newer than the one answering.
    ///
    /// An absent hold is honest about two different things at once — nothing is
    /// held, or the answering router has no dispatchers behind it to hold. A
    /// router with no dispatchers is not holding anything back either way.
    #[serde(default)]
    pub github: Option<GitHubHold>,
    /// Set while an upgrade is half-applied — a newer server binary on disk
    /// awaiting `make restart`, or a VM image observed running a build older
    /// than this server's, awaiting `make images`. While set (and enforced),
    /// no new scout or build starts; in-flight work runs to completion and
    /// queued work stays queued. `#[serde(default)]` for the same reload-skew
    /// reason as the fields above.
    #[serde(default)]
    pub update: Option<UpdatePending>,
    /// Set for as long as scout and build dispatch is being held because
    /// vm-pool has no free slot. `#[serde(default)]` for the same reload-skew
    /// reason as the fields above.
    #[serde(default)]
    pub pool: Option<PoolHold>,
    /// Set for as long as scout and build dispatch is being held because the
    /// credential broker is not answering. `#[serde(default)]` for the same
    /// reload-skew reason as the fields above.
    #[serde(default)]
    pub broker: Option<BrokerHold>,
    /// Set for as long as scout and build dispatch is being held because this
    /// host's container runtime is not running. `#[serde(default)]` for the
    /// same reload-skew reason as the fields above.
    #[serde(default)]
    pub runtime: Option<RuntimeHold>,
    /// How big the orchestrator's warm verification build directory is, as
    /// last measured. `#[serde(default)]` for the same reload-skew reason as
    /// the fields above.
    ///
    /// **Unlike the three holds, this is present whenever there is a reading**,
    /// not only when something is wrong. A hold is an exception and a row that
    /// only appears in the bad state is one a reader learns to skip; this is a
    /// quantity that grows silently, and a report that appeared only once it
    /// was over its ceiling would reproduce #1010 — 51 GB found by a human
    /// hunting for disk — exactly.
    ///
    /// `None` means no reading: the orchestrator has no checkout to build in,
    /// this router has no orchestrator behind it, or the first walk has not
    /// happened yet.
    #[serde(default)]
    pub verify_dir: Option<VerifyDirUsage>,
}

/// Why the pipeline is idle when the container runtime is not running.
///
/// Nothing here can start a VM, so work dispatched into it fails at the
/// allocate and is charged an attempt for it — 3 builds and 12 tasks in one
/// play window on 2026-08-19, after a reboot left apple/container's apiserver
/// unregistered with launchd (#1017). Holding costs nothing, and the discharge
/// is one command: `container system start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeHold {
    /// The first failed probe in this run — "down since", which later probes
    /// do not move.
    pub since: DateTime<Utc>,
    /// The most recent failed probe.
    pub last_seen: DateTime<Utc>,
    /// How many probes have failed since `since`.
    pub probes: u32,
    /// What `container system status` said — the one thing that distinguishes
    /// a stopped service from a broken install.
    pub error: String,
}

/// Why the pipeline is idle when the credential broker is not answering.
///
/// Every clone inside a VM is redeemed against the broker, so a broker that
/// stops answering fails every scout and every build at the clone — a
/// pre-agent setup failure, which the strike rule charges deliberately. An
/// outage of one minute therefore does not delay work, it destroys it: two
/// tasks went from `queued` to `rejected` in 27 and 43 seconds on 2026-08-18
/// (#1006). Holding costs nothing: queued work stays queued, and the next
/// probe that gets a `401` releases it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerHold {
    /// The first failed probe in this run — "down since", which later probes
    /// do not move.
    pub since: DateTime<Utc>,
    /// The most recent failed probe. The gap between this and now is the
    /// difference between a hold a dispatcher is still refreshing and one
    /// about to expire on its own.
    pub last_seen: DateTime<Utc>,
    /// How many probes have failed since `since`.
    pub probes: u32,
    /// The advertised address that was probed (`TASKS_BROKER_ADVERTISE` and
    /// `TASKS_BROKER_PORT`) — never loopback, which answers correctly while
    /// the bridge gateway is severed. Carried so the report names the thing to
    /// check rather than the concept.
    pub address: String,
    /// The most recent failure, rendered — prose for a reader.
    pub error: String,
}

/// How big the orchestrator's verification build directory is, and what bounds
/// it (`ORCHESTRATOR_TARGET_DIR`, `ORCHESTRATOR_TARGET_BUDGET_GB`).
///
/// Measured on a cadence rather than at request time — the walk is hundreds of
/// thousands of files — so `measured_at` is part of the answer rather than
/// decoration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyDirUsage {
    /// Absolute path **on the server host**, like [`RejectedBundle::path`].
    pub path: String,
    pub bytes: u64,
    /// Files counted, a hardlinked file once — cargo hardlinks
    /// `<profile>/<bin>` to `<profile>/deps/<bin>-<hash>`, and a number that
    /// disagreed with `du -sh` would not be trusted twice.
    pub files: u64,
    pub measured_at: DateTime<Utc>,
    /// The ceiling in bytes, or `None` for `ORCHESTRATOR_TARGET_BUDGET_GB=0`:
    /// report only, no reclaim. The report half is deliberately not
    /// switchable.
    pub budget_bytes: Option<u64>,
    pub over_budget: bool,
    /// The last reclaim of this boot, kept for the whole boot: the wholesale
    /// tier costs the next verification a cold build, and that cost has to
    /// still be answerable after the event feed has scrolled.
    pub last_reclaim: Option<VerifyDirReclaim>,
}

/// A reclaim that happened, with both numbers measured rather than estimated —
/// each tier re-walks the directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyDirReclaim {
    pub at: DateTime<Utc>,
    pub tier: VerifyDirTier,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// How far a reclaim had to go — which is also what it cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyDirTier {
    /// Every `<profile>/incremental`, which is keyed to one worktree path and
    /// therefore costs no warmth. On the host measured on 2026-08-20 this was
    /// 24.24 GB of 51.
    Incremental,
    /// The directory's contents. **The next verification is cold**, which is
    /// minutes of compilation before a single test runs.
    Wholesale,
}

/// Why the pipeline is idle when vm-pool has no room.
///
/// A dispatch into a full pool is refused, and a refused Scout used to be both
/// charged an attempt and stranded in `Scouting` until the next boot (#930,
/// #967). Holding costs nothing: queued work stays queued, and the next VM
/// handed back releases it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolHold {
    /// When the pool was first observed full — "no room since", which later
    /// observations do not move.
    pub since: DateTime<Utc>,
    /// The most recent observation. The gap between this and now is the
    /// difference between a hold a dispatcher is still refreshing and one
    /// about to expire on its own.
    pub last_seen: DateTime<Utc>,
    /// How many observations have been made since `since`.
    pub observations: u32,
    /// Slots the pool holds in total, so a reader can tell `0 of 0` — a
    /// `VM_POOL_MAX_VMS` that can never dispatch — from `0 of 6`, which is work
    /// or a leak holding every slot.
    pub total: usize,
}

/// Why new containers are waiting: an upgrade is half-applied.
///
/// Each reason names its own discharge (`make restart` / `make images`),
/// because the reader's next question is always "what do I run".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePending {
    pub reasons: Vec<String>,
    /// Whether the hold is binding dispatch, or merely being reported
    /// (`TASKS_UPDATE_HOLD=off`).
    pub enforced: bool,
}

/// Why the pipeline is idle when GitHub is not answering.
///
/// A Scout clones and a Builder clones, so work dispatched into an outage dies
/// at its first step and is charged an attempt for it. Holding costs nothing:
/// queued work stays queued, and the next poll that succeeds releases it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubHold {
    /// The first failed call in this run — "down since", which later failures
    /// do not move.
    pub since: DateTime<Utc>,
    /// The most recent failed call. The gap between this and now is the
    /// difference between a hold somebody is still refreshing and one about to
    /// expire on its own.
    pub last_seen: DateTime<Utc>,
    /// How many calls have failed since `since`.
    pub failures: u32,
    /// The most recent failure, rendered — prose for a reader.
    pub error: String,
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

/// A build whose branch could not be pushed, and whose commits were written
/// down instead — one entry of `GET /bundles`, and the whole of
/// `GET /builds/{id}/bundle`.
///
/// **Derived, never stored.** Everything here is read at request time: the
/// file's size and mtime from `read_dir`, the rest from the build row. There
/// is no bundles table and no cached size, because the directory is one a
/// human works in — recovering a bundle means `cd`-ing there and running git —
/// and a row asserting a file exists goes stale the moment somebody `rm`s one.
///
/// The bundle is **thin**: it carries the build's commits and not the commit
/// they grew from, so [`Self::recovery_command`] only reconstructs the branch
/// in a repository that already has `base_sha`. A clone of the trunk normally
/// does; a build stacked on another build's branch may not — which is why
/// `base_sha` is reported beside the command rather than left implied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedBundle {
    pub build_id: BuildId,
    /// Absolute path **on the server host**. The app is loopback-local today,
    /// but this is still a path in someone else's filesystem as far as any
    /// client is concerned — which is why recovery is a command to run and
    /// not a button to press.
    pub path: String,
    pub bytes: u64,
    /// The file's mtime: when egress failed and the bundle was written.
    pub created_at: DateTime<Utc>,
    pub branch: String,
    /// The commit the branch grew from — the bundle's prerequisite.
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// Why the build failed, as the build row records it. The reason egress
    /// was refused is the first thing a human needs in order to decide
    /// whether recovering is worth it.
    pub exit_reason: Option<String>,
    /// The tasks this build implements. A build that never landed a branch has
    /// no PR and appears nowhere else in a UI, so the bundle is shown against
    /// the *work* rather than against an id nobody recognises. Computed
    /// server-side because a client has no build→spec→task join to do it with.
    pub task_ids: Vec<TaskId>,
    /// The `git fetch` that gets the work back, shell-quoted and complete.
    pub recovery_command: String,
    /// Whether the retention policy would reclaim this: every spec in the
    /// batch carried by a later build that succeeded, and every task in it
    /// `done`. Reported so a human deleting one by hand can see whether they
    /// are throwing away the only copy of something.
    pub superseded: bool,
}

/// The answer to `GET /viewer`: who the server's own GitHub credential is.
///
/// Not a login flow. The identity that matters to this system is the one whose
/// branches get pushed and whose issues get closed, and that is exactly the
/// account the server's token names — a second, client-side identity beside it
/// would be a second answer to a question with one.
///
/// **An enum rather than a struct of `Option`s**, and that is the load-bearing
/// choice: a half-identity — a login with no avatar, an avatar with no profile
/// to open — is unrepresentable, and every renderer is forced by the compiler
/// to answer all three cases rather than defaulting two of them into a broken
/// image and a link to nowhere.
///
/// The route always answers 200. All three are *states this route reports*,
/// not failures of it: a fresh machine with no token is the common case, and a
/// 503 there would put a red banner on an app that is working correctly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Viewer {
    /// GitHub answered, and every field it needs came back.
    Known {
        login: String,
        avatar_url: String,
        /// **GitHub's own `url` field**, never `https://github.com/{login}`
        /// assembled from the login: on GitHub Enterprise the origin is not
        /// github.com, and a link built that way opens the wrong host. That
        /// guess is the same class of mistake that produced #987.
        profile_url: String,
    },
    /// No credential resolves, so nothing was asked. Not an error — the
    /// ordinary state of a server nobody has sealed a token into yet.
    Unauthenticated,
    /// A credential exists and GitHub did not answer with an identity.
    ///
    /// `error` is **GitHub's own response message and nothing else** — never a
    /// URL, a header, or a transport error that quotes what it was sent. It is
    /// rendered in a tooltip, and a tooltip is output: #971's rule that no
    /// credential or fragment of one reaches output applies here as much as to
    /// a log line.
    Unknown { error: String },
}

impl Viewer {
    /// The account name, when there is one.
    pub fn login(&self) -> Option<&str> {
        match self {
            Self::Known { login, .. } => Some(login),
            _ => None,
        }
    }

    /// Where to fetch the avatar image, when there is one.
    pub fn avatar_url(&self) -> Option<&str> {
        match self {
            Self::Known { avatar_url, .. } => Some(avatar_url),
            _ => None,
        }
    }

    /// Where clicking the avatar goes, when there is anywhere to go.
    pub fn profile_url(&self) -> Option<&str> {
        match self {
            Self::Known { profile_url, .. } => Some(profile_url),
            _ => None,
        }
    }

    /// One sentence naming which of the three states this is — what a human
    /// reads under the cursor, and what a diagnostic prints.
    pub fn describe(&self) -> String {
        match self {
            Self::Known { login, .. } => format!("{login} on GitHub"),
            Self::Unauthenticated => "Not signed in — no GitHub token configured".to_string(),
            Self::Unknown { error } => format!("GitHub identity unavailable: {error}"),
        }
    }
}

/// One entry of `GET /labels`: the repository's label vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelInfo {
    pub name: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::{AppliedMigration, Viewer};

    fn known() -> Viewer {
        Viewer::Known {
            login: "octocat".into(),
            avatar_url: "https://avatars.example/u/1".into(),
            profile_url: "https://github.example/octocat".into(),
        }
    }

    /// The tag is what a client matches on, and the three names are wire
    /// vocabulary — renaming one is a client-visible break, so it is pinned.
    #[test]
    fn each_state_is_tagged_by_name() {
        let json = |v: &Viewer| serde_json::to_value(v).unwrap();
        assert_eq!(json(&known())["state"], "known");
        assert_eq!(json(&Viewer::Unauthenticated)["state"], "unauthenticated");
        assert_eq!(
            json(&Viewer::Unknown {
                error: "Bad credentials".into()
            })["state"],
            "unknown"
        );
    }

    /// Round trip, because the app deserializes exactly what the server wrote.
    #[test]
    fn a_known_viewer_round_trips_every_field() {
        let encoded = serde_json::to_string(&known()).unwrap();
        assert_eq!(serde_json::from_str::<Viewer>(&encoded).unwrap(), known());
    }

    /// Only `Known` has anywhere to click. This is what makes the chip inert
    /// in every fallback state rather than a link to a broken page.
    #[test]
    fn only_a_known_viewer_offers_a_profile() {
        assert_eq!(
            known().profile_url(),
            Some("https://github.example/octocat")
        );
        assert_eq!(known().login(), Some("octocat"));
        assert_eq!(known().avatar_url(), Some("https://avatars.example/u/1"));
        for other in [
            Viewer::Unauthenticated,
            Viewer::Unknown {
                error: "Bad credentials".into(),
            },
        ] {
            assert_eq!(other.profile_url(), None);
            assert_eq!(other.login(), None);
            assert_eq!(other.avatar_url(), None);
        }
    }

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
