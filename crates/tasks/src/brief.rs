//! Computed briefs: the facts a decision needs, gathered by the server.
//!
//! The orchestrator's context is the budget its judgment is bought with, and
//! until now it spent that budget foraging — paging the whole event log to
//! re-derive what was in flight, and remembering (or not) that some other spec
//! touched the same files. Foraging is expensive and unreliable in the same
//! direction: the checks it skips are the ones nobody notices were skipped.
//!
//! So the server runs them instead. A brief is *facts*, never a score and
//! never a verdict — "this spec shares four files with PR #806, which is open"
//! is a fact; whether that means duplication or coincidence is judgment, and
//! judgment stays with the agent holding the accumulated context. The design
//! rule is that every line here is something a careful reviewer would have
//! gone and looked up, rendered in fewer tokens than looking it up would cost.
//!
//! Briefs are attached to the turns where a decision is actually made — specs
//! landing, obligations coming due — rather than emitted on a schedule, so
//! their cost tracks decisions rather than time.
//!
//! GitHub facts (PR state, what is already on the base branch) are read live
//! per the "never persist a GitHub-owned fact" rule, and are best-effort: when
//! GitHub is slow or down the brief says so rather than quietly shrinking,
//! because a missing line and a clean check must not look alike.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::builder::summary_accounts_for_review_feedback;
use crate::github::GitHubClient;
use crate::github_health::GitHubHealth;
use crate::models::{
    Build, BuildId, BuildStatus, Project, ProjectId, Spec, SpecId, SpecQueueEntry, SpecQueueStatus,
    Task, TaskId, TaskState, Verification, VerificationStatus,
};
use crate::store::{Store, StoreError};

/// Whole-brief budget for GitHub reads. Past this the DB-derived facts still
/// ship — a late brief is worse than a partial one, since the turn it rides on
/// is what makes work happen.
///
/// **Whole-brief, and enforced as one deadline** ([`Brief::within_github_budget`]):
/// a per-call timeout of the same size lets a turn briefing four subjects spend
/// forty seconds, which is not what this number says.
const GITHUB_BUDGET: Duration = Duration::from_secs(10);

/// The part of the tree nothing runnable on a Linux builder can *see*.
///
/// It compiles and unit-tests here — `make app-check` / `make app-test`, since
/// #877/#893 put the five packages in the images — but those tests are pure
/// functions over view state, so whether a pixel landed anywhere is still a
/// `make app` on a Mac. Naming the boundary this precisely is what keeps the
/// rule narrow: "the GUI is unreviewable" is both false and, being false,
/// ignored.
const VISUAL_SURFACE: &str = "app-gpui/";

/// Most overlapping paths to name before summarizing the rest. Enough to
/// recognize *what* overlaps; the count carries the scale.
const MAX_NAMED_FILES: usize = 4;

/// How many past decisions on the same task to recall.
const MAX_PRIOR_DECISIONS: usize = 3;

/// A build stops being context this long after it finished — the outer bound
/// on what a brief will even consider, not the test for whether a build is
/// still live. That test is [`is_unresolved`], which reads the batch's own
/// state; this only keeps the window a brief loads bounded.
const BUILD_RECENCY_DAYS: i64 = 14;

/// Digits needed before a filename counts as sequence-numbered. Three is
/// enough to exclude ordinary names that merely start with a digit
/// (`2fa_login.rs`) while catching the numbering schemes people actually
/// collide on.
const MIN_SEQUENCE_DIGITS: usize = 3;

pub struct Brief<'a> {
    store: &'a Store,
    /// `None` when the server has no token — every GitHub-derived fact is then
    /// unavailable, and the brief says so.
    github: Option<&'a GitHubClient>,
    /// The branch specs are written against, and therefore the one to compare
    /// against ([`crate::run::Config::scout_base_branch`]).
    base_branch: &'a str,
    /// Whether GitHub is answering, as last observed by the poller. `None`
    /// where nothing is observing — the two unit-test call sites and any
    /// caller built before the record existed — and an unobserved record says
    /// nothing, exactly as an unobserved image does.
    github_health: Option<&'a GitHubHealth>,
    /// Read on first use and shared by every fact on this brief. A turn
    /// briefing three specs plus the pipeline reads the tables once, not four
    /// times, and — more importantly — every line in one `[brief]` block
    /// describes the same instant.
    world: tokio::sync::OnceCell<World>,
    /// Set at the first GitHub read and shared by every later one, so
    /// [`GITHUB_BUDGET`] bounds the brief rather than each call inside it.
    github_deadline: tokio::sync::OnceCell<tokio::time::Instant>,
}

/// Everything the DB half of a brief reads, loaded once per brief rather than
/// once per fact. Small tables (this is a single-operator tool), and reading
/// them together keeps the fact functions pure and testable.
struct World {
    specs: Vec<Spec>,
    /// The whole queue entry, not just its status: a stranded build's brief
    /// needs to know whether the batch carried review feedback, and that lives
    /// on the same row.
    queue: HashMap<SpecId, SpecQueueEntry>,
    tasks: HashMap<TaskId, Task>,
    projects: HashMap<ProjectId, Project>,
    builds: Vec<Build>,
    /// Spec ids per build, for naming what a build is carrying.
    build_specs: HashMap<String, Vec<SpecId>>,
    /// Builds whose every spec a later succeeded build has carried. They are
    /// settled however their own pull request reads — see
    /// [`Store::builds_superseded`] and [`is_unresolved`].
    superseded: HashSet<String>,
}

impl<'a> Brief<'a> {
    pub fn new(store: &'a Store, github: Option<&'a GitHubClient>, base_branch: &'a str) -> Self {
        Self {
            store,
            github,
            base_branch,
            github_health: None,
            world: tokio::sync::OnceCell::new(),
            github_deadline: tokio::sync::OnceCell::new(),
        }
    }

    /// Read the dispatch hold from the record the poller writes.
    ///
    /// A builder method rather than a fourth parameter on [`Self::new`]:
    /// every caller that has a health record is a serving one, and the unit
    /// tests that do not have to stay constructible without inventing a fake.
    pub fn with_github_health(mut self, health: &'a GitHubHealth) -> Self {
        self.github_health = Some(health);
        self
    }

    /// Facts for judging one spec: what else claims its files, what was
    /// already decided about its task, and what is already on the base branch
    /// where it proposes to write.
    ///
    /// An empty result means every check came back clean, which is worth
    /// distinguishing from "no checks ran" — see [`Self::render`].
    pub async fn for_spec(&self, spec_id: &SpecId) -> Result<Vec<String>, StoreError> {
        let Some(spec) = self.store.get_spec(spec_id).await? else {
            return Ok(Vec::new());
        };
        let world = self.world().await?;
        let mut lines = Vec::new();

        lines.extend(spec_overlap(&spec, world));
        lines.extend(build_overlap(&spec, world));
        lines.extend(sequence_clash_in_flight(&spec, world));
        lines.extend(self.prior_decisions(&spec, world).await?);
        lines.extend(self.github_facts(&spec, world).await);

        Ok(lines)
    }

    /// Facts for a spec that ran out of build attempts: what actually failed,
    /// each time. The obligation says it stopped; this says why, which is the
    /// whole difference between deciding and guessing.
    pub async fn for_blocked_spec(&self, spec_id: &SpecId) -> Result<Vec<String>, StoreError> {
        let world = self.world().await?;
        let mut lines = Vec::new();
        for build in world.builds.iter().filter(|b| {
            b.status == BuildStatus::Failed
                && world
                    .build_specs
                    .get(build_key(b))
                    .is_some_and(|ids| ids.contains(spec_id))
        }) {
            lines.push(format!(
                "build {} failed: {}",
                build.id,
                build
                    .exit_reason
                    .as_deref()
                    .unwrap_or("no reason recorded — check the build row"),
            ));
        }
        if lines.is_empty() {
            lines.push(
                "no failed build rows reference this spec — it was blocked without an \
                 attempt on record, which is itself worth looking into"
                    .into(),
            );
        }
        Ok(lines)
    }

    /// Facts for a decision whose effect nobody confirmed: what was about to
    /// be done, where, and what came back.
    ///
    /// It ends by naming the two calls that discharge it, in order, because
    /// the whole point of `ObligationKind::ReconcileDecision` is that its
    /// recipient can settle it from evidence rather than from a guess — and
    /// the evidence comes from the server, which holds the credential.
    pub async fn for_pending_decision(&self, seq: i64) -> Result<Vec<String>, StoreError> {
        let Some(decision) = self.store.decision(seq).await? else {
            return Ok(vec![format!(
                "decision {seq} is not in the ledger — the obligation named a row that is gone"
            )]);
        };
        let mut lines = vec![format!(
            "decision {seq}: {} on {} {}, by {}, recorded {}",
            decision.action.as_str(),
            decision.subject_kind,
            decision.subject_id,
            decision.actor.as_str(),
            decision.created_at.format("%Y-%m-%d %H:%M UTC"),
        )];
        if decision.state != crate::models::DecisionState::Pending {
            lines.push(format!(
                "it is already {} — nothing to reconcile",
                decision.state.as_str()
            ));
            return Ok(lines);
        }
        if let Some(rationale) = decision.rationale.as_deref() {
            lines.push(format!("its stated reason: {rationale}"));
        }
        if let Some(intent) = decision.outcome.as_ref().and_then(|o| o.get("intent")) {
            lines.push(format!("what it was about to do: {intent}"));
        }
        if let Some(unanswered) = decision.outcome.as_ref().and_then(|o| o.get("unanswered")) {
            lines.push(format!(
                "GitHub never answered: {unanswered} — so the write may or may not have landed"
            ));
        }
        // Named even when the capability is `live`, because the case that
        // matters is the demoted one: settling is deliberately not gated, and
        // a reader who assumes it is would leave this row open forever.
        if let Some(capability) = decision.action.capability() {
            let level = self.store.charter_entry(capability).await?.level;
            lines.push(format!(
                "it came from `{}`, currently {} — settling is not gated by the charter either \
                 way: the effect already happened, and refusing to record it would only keep \
                 the ledger wrong",
                capability.as_str(),
                level.as_str(),
            ));
        }
        lines.push(format!(
            "to discharge: GET /decisions/{seq}/reconcile — the server asks GitHub with its own \
             credential and tells you what it found — then POST /decisions/{seq}/settle \
             {{\"state\":\"applied|annulled\",\"rationale\",\"outcome\"}}. If the lookup \
             answers `unknown`, leave it pending: that is the honest state, and a guess written \
             into an append-only ledger is worse than the missing row this exists to prevent"
        ));
        Ok(lines)
    }

    /// Facts for a batch parked behind a pull request that has not landed:
    /// which branch it is, what it merges *into*, and whose issues are waiting.
    ///
    /// The base is the point. A build stacked on another build's branch merges
    /// cleanly, reads `merged: true`, and ships nothing until that branch
    /// itself reaches the trunk — so when the base is not the trunk this spells
    /// the trap out rather than leaving it to be inferred from a branch name.
    /// It is how PR #863 was lost.
    ///
    /// Three further lines answer the three questions that decide whether this
    /// PR can be landed at all: what GitHub says about the merge, what the
    /// build claimed about its own test run, and how much of the batch nothing
    /// runnable here could have checked. All three are facts — who acts on
    /// them is stated once, in the generated `land_batch` section of the
    /// orchestrator's prompt, and nowhere else.
    ///
    /// A fourth appears only when this batch's specs were approved *with*
    /// review feedback: whether the build accounted for it. See
    /// [`review_feedback_line`] — it is a fact and not a fourth landing
    /// carve-out, and says so in its own wording.
    pub async fn for_stranded_build(&self, build_id: &BuildId) -> Result<Vec<String>, StoreError> {
        let Some(build) = self.store.get_build(build_id).await? else {
            return Ok(vec![format!(
                "build {build_id} is not in the store — the obligation named a row that is gone"
            )]);
        };
        let world = self.world().await?;
        // What the build was *cloned against*, stated as the build fact it is.
        // Whether that is still the pull request's base is GitHub's to say, so
        // the stacked verdict is not derived here — it comes from `base_ref` in
        // [`Self::live_landing_facts`], where the pull request is in hand.
        let mut lines = vec![format!(
            "branch {} was built against {}, and PR #{} carries it",
            build.branch,
            build.base_branch,
            build
                .pr_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(none)".into()),
        )];
        let waiting: Vec<String> = world
            .build_specs
            .get(build_key(&build))
            .into_iter()
            .flatten()
            .filter_map(|spec_id| world.specs.iter().find(|s| &s.id == spec_id))
            .filter_map(|spec| world.tasks.get(&spec.task_id))
            .filter(|task| task.state == TaskState::AwaitingMerge)
            .map(|task| format!("#{}", task.gh_issue_number))
            .collect();
        if !waiting.is_empty() {
            lines.push(format!("still awaiting merge: {}", waiting.join(", ")));
        }
        lines.push(verification_line(build.verification.as_ref()));
        lines.extend(review_feedback_line(&build, world));
        lines.push(verification_surface(&build.files_touched));
        lines.extend(self.landing_facts(&build, world).await);
        Ok(lines)
    }

    /// What the pipeline is doing right now, in the two or three lines that
    /// change a judgment: what a Builder is holding, and what is already
    /// approved and waiting behind it.
    pub async fn pipeline(&self) -> Result<Vec<String>, StoreError> {
        let world = self.world().await?;
        // First, because it changes how every line under it reads: a queue
        // that is not moving during a hold is not a queue that is stuck.
        let mut lines = self.github_hold_facts(Utc::now());

        let in_flight: Vec<&Build> = world
            .builds
            .iter()
            .filter(|b| matches!(b.status, BuildStatus::Queued | BuildStatus::Running))
            .collect();
        // A spec stays `approved` for the whole build that carries it —
        // `create_build` deliberately leaves the queue status alone so a failed
        // build can return its specs for another attempt. So "approved" does
        // not mean "unbuilt", and the waiting count has to subtract what the
        // lines above just named, or the same specs appear on both. `queued`
        // counts as carried alongside `running` because builds are serial: a
        // dispatched batch sitting behind the running one has already been
        // asked for. Same rule as `Store::obligations` and the guard in
        // `Store::create_build`; the set is built from the same iteration that
        // prints the lines so the two cannot disagree.
        let mut carried: HashSet<&SpecId> = HashSet::new();
        for build in in_flight {
            lines.push(format!(
                "build {} is {} on branch {} ({})",
                build.id,
                build.status.as_str(),
                build.branch,
                self.describe_specs(build_key(build), world),
            ));
            if let Some(ids) = world.build_specs.get(build_key(build)) {
                carried.extend(ids);
            }
        }

        let approved = world
            .queue
            .iter()
            .filter(|(spec_id, entry)| {
                entry.status == SpecQueueStatus::Approved && !carried.contains(spec_id)
            })
            .count();
        if approved > 0 {
            lines.push(format!(
                "{approved} approved spec(s) are in no build yet, waiting to be batched \
                 into a Builder run"
            ));
        }

        // What is parked behind an open PR, read from the store rather than
        // from `world.builds`: `load_world` drops anything finished more than
        // BUILD_RECENCY_DAYS ago, which is exactly the batch that has been
        // stranded longest. This is also the query `run::watch_merges` walks,
        // so the line and the poll cannot describe different sets.
        for project in world.projects.values() {
            for parked in self.store.list_builds_awaiting_merge(&project.id).await? {
                let issues: Vec<String> = parked
                    .tasks
                    .iter()
                    .map(|t| format!("#{}", t.gh_issue_number))
                    .collect();
                lines.push(format!(
                    "build {} is parked behind PR #{}: {} still awaiting merge",
                    parked.build_id,
                    parked.pr_number,
                    issues.join(", "),
                ));
            }
        }

        lines.extend(self.stale_image_facts().await?);
        Ok(lines)
    }

    /// Whether GitHub is answering, when it is not.
    ///
    /// #939 shipped the hold and reported it to `/status`, `tasks status` and
    /// the Server window, and argued the orchestrator out of the picture: it
    /// cannot fix GitHub, an obligation it can never discharge is raised every
    /// pass forever, and a `Note` is deliberately not nudge-worthy. Every
    /// clause of that is right and none of it reaches this, because **a brief
    /// line is not an obligation** — the precedent is one function down, where
    /// a stale image is reported on the brief with no `ObligationKind` behind
    /// it, for exactly the same reason: worth showing, not worth waking
    /// anyone for.
    ///
    /// It matters more here than there, because during an outage the
    /// orchestrator is not a bystander. It is the process still issuing GitHub
    /// writes *through the server* — merges, comments, closes, issue edits —
    /// over the same API that is returning 503, and it does not read `/status`
    /// unprompted. The brief is what it reads.
    ///
    /// So the wording is what the hold actually is: a hold on **dispatch**,
    /// which says nothing about whether any given write will succeed. The
    /// reading to produce is "expect your writes to fail and stop retrying",
    /// never "you are blocked" — the orchestrator has plenty it can still do,
    /// and a line that reads as a stop order costs a turn.
    ///
    /// Silent when there is no hold, like every other renderer of this record,
    /// so it costs nothing on a healthy pipeline. It reads
    /// [`GitHubHealth::hold`] — the same predicate the two dispatchers and
    /// `/status` read, bound to the same staleness window — rather than
    /// deciding freshness a second time.
    fn github_hold_facts(&self, now: DateTime<Utc>) -> Vec<String> {
        let Some(outage) = self.github_health.and_then(|h| h.hold(now)) else {
            return Vec::new();
        };
        vec![format!(
            "GitHub has not answered since {} ({} failed call(s); latest: {}). Scout and \
             build dispatch is held until it does — queued work stays queued and nothing \
             is charged an attempt. This is not a hold on you: it means your own writes \
             through the server (merges, comments, closes, issue edits) are going to the \
             same API and are likely to fail, so read a failure as the outage rather than \
             as a refusal, and do not retry it this turn",
            outage.since.to_rfc3339(),
            outage.failures,
            outage.error,
        )]
    }

    /// What the VM images are running, when that changes how a failure should
    /// be read.
    ///
    /// A **fact, not an obligation**, and deliberately so — but not because
    /// the orchestrator lacks the means. It routinely runs in the checkout
    /// with the `container` CLI and the cross toolchain within reach, so the
    /// honest reason is what a rebuild *decides*: it is a deployment, changing
    /// what every future run executes with no review in front of it and no
    /// revert but another rebuild, which puts it in the human-only
    /// `build-now` category. The decision is a human's however the host is
    /// configured, so an obligation would be undischargeable by the party it
    /// is addressed to, raised every pass forever — which is how a signal gets
    /// trained out of use.
    ///
    /// Worded around the judgment the orchestrator actually makes, rather than
    /// around the rebuild it cannot perform: a run that failed inside a stale
    /// image has not told you anything about its task. That is the reading
    /// #884 got wrong.
    ///
    /// Silent when every image is current, and silent when none has been
    /// observed — an unobserved image is not a fact about anything yet, and
    /// the standing `/status` line is where "nothing observed" belongs.
    async fn stale_image_facts(&self) -> Result<Vec<String>, StoreError> {
        let mut lines = Vec::new();
        for image in self.store.image_builds(crate::version::VERSION).await? {
            if !image.freshness.needs_rebuild() {
                continue;
            }
            let running = match &image.version {
                Some(version) => format!("is running supervisor {version}"),
                None => "predates supervisor stamping".to_string(),
            };
            lines.push(format!(
                "the {} VM image ({}) {}, while this server is {} — a run that failed inside \
                 it may have died of a bug already fixed here, which is not a verdict on its \
                 task. Only a human at the host can rebuild it (`make images`)",
                image.role.as_str(),
                image.image,
                running,
                crate::version::VERSION,
            ));
        }
        Ok(lines)
    }

    /// Render brief lines as the block that rides along on a turn.
    ///
    /// Says what was checked even when nothing was found, because the useful
    /// reading of a quiet brief is "these checks came back clean" and the
    /// dangerous one is "the checks did not run".
    pub fn render(sections: &[(String, Vec<String>)]) -> Option<String> {
        if sections.iter().all(|(_, lines)| lines.is_empty()) {
            return None;
        }
        let mut out = String::from(
            "[brief] Facts the server computed for this turn — not a verdict, and not \
             a substitute for reading the spec. These are the lookups you would \
             otherwise run yourself; anything not listed was not checked.\n",
        );
        for (heading, lines) in sections {
            if lines.is_empty() {
                continue;
            }
            out.push_str(heading);
            out.push('\n');
            for line in lines {
                out.push_str("  - ");
                out.push_str(line);
                out.push('\n');
            }
        }
        Some(out.trim_end().to_string())
    }

    // --- fact gathering ---

    /// The DB snapshot every fact on this brief reads from, loaded at most
    /// once.
    async fn world(&self) -> Result<&World, StoreError> {
        self.world.get_or_try_init(|| self.load_world()).await
    }

    /// Run one GitHub read inside the brief's *shared* [`GITHUB_BUDGET`],
    /// answering `None` when it did not fit.
    ///
    /// The deadline is taken at the first read and reused, so a turn briefing
    /// four subjects still spends ten seconds on GitHub in total. The explicit
    /// expiry check ahead of `timeout_at` is what makes an exhausted budget
    /// answer *without issuing a request*: `timeout_at` polls its inner future
    /// once before consulting the timer, and that first poll is the one that
    /// opens the connection.
    async fn within_github_budget<F: std::future::Future>(&self, fut: F) -> Option<F::Output> {
        let deadline = *self
            .github_deadline
            .get_or_init(|| async { tokio::time::Instant::now() + GITHUB_BUDGET })
            .await;
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::timeout_at(deadline, fut).await.ok()
    }

    /// The live half of a stranded build's brief: what GitHub says about
    /// landing this pull request, and — only for a stacked build — where its
    /// base stands relative to the trunk.
    ///
    /// Every guard gets its own line rather than falling through to silence.
    /// A missing fact and a clean one read identically otherwise, and of the
    /// two only one of them is safe to act on.
    async fn landing_facts(&self, build: &Build, world: &World) -> Vec<String> {
        let Some(number) = build.pr_number else {
            return vec![
                "no pull request is recorded on this build, so there is nothing to land \
                 here — the batch is parked on a claim nothing was opened for"
                    .into(),
            ];
        };
        let Some(github) = self.github else {
            return vec![
                format!(
                    "GitHub was not consulted (the server has no token): whether PR #{number} \
                     can be merged is unchecked rather than fine"
                ),
                recorded_base_verdict(&build.base_branch, self.base_branch),
            ];
        };
        let Some(project) = world.projects.get(&build.project_id) else {
            return vec![
                format!(
                    "the project row for this build is gone, so PR #{number} could not be \
                     looked up"
                ),
                recorded_base_verdict(&build.base_branch, self.base_branch),
            ];
        };
        match self
            .within_github_budget(self.live_landing_facts(github, project, build, number))
            .await
        {
            Some(lines) => lines,
            None => {
                warn!(build = %build.id, number, "landing facts timed out; briefing without them");
                vec![
                    format!(
                        "GitHub did not answer within the brief's {}s budget: whether PR \
                         #{number} can be merged is unchecked rather than fine",
                        GITHUB_BUDGET.as_secs()
                    ),
                    recorded_base_verdict(&build.base_branch, self.base_branch),
                ]
            }
        }
    }

    async fn live_landing_facts(
        &self,
        github: &GitHubClient,
        project: &Project,
        build: &Build,
        number: u64,
    ) -> Vec<String> {
        let owner = &project.repo_owner;
        let name = &project.repo_name;
        let pr = match github.pull_request_state(owner, name, number).await {
            Ok(pr) => pr,
            Err(e) => {
                debug!(error = %e, number, "pr state unavailable");
                return vec![
                    format!(
                        "PR #{number} could not be read ({e}): its mergeability is unknown \
                         rather than fine"
                    ),
                    recorded_base_verdict(&build.base_branch, self.base_branch),
                ];
            }
        };
        let mut lines = vec![format!(
            "PR #{number} is {}: {}",
            pr.label(),
            pr.landing().describe()
        )];

        // The stacked question, answered from `base_ref` — GitHub's own field,
        // never `builds.base_branch`.
        //
        // The column is the honest record of what the build was *cloned
        // against*, and that is a different question: a human or `gh pr edit`
        // can retarget a pull request at any moment and nothing updates the
        // column when they do, so a retargeted PR read from the column stays
        // "stacked" forever. That is #1035, and its failure direction is the
        // bad one — the stacked paragraph is a reason *not* to merge, so a
        // stale base makes the brief argue for parking a pull request that is
        // ready, and tell its reader to redo a retarget that already happened.
        // `run::shipped` has always read `base_ref`; one question gets one
        // source, or the poller and the brief disagree about the same PR.
        //
        // Absent is never "unstacked": GitHub not reporting a base is unknown,
        // and the standing rule is that absence of evidence never clears.
        let Some(pr_base) = pr.base_ref.clone() else {
            lines.push(
                "GitHub did not report this PR's base branch, so whether it is stacked is \
                 unknown rather than fine — treat it as unchecked"
                    .into(),
            );
            return lines;
        };
        if pr_base != build.base_branch {
            lines.push(format!(
                "this PR has been retargeted since the build was made: it was built against \
                 {}, and its base is now {} — everything below reads GitHub's base, not the \
                 build's",
                build.base_branch, pr_base
            ));
        }
        // An unstacked PR has its answer here and must cost no extra call.
        lines.push(base_verdict(&pr_base, self.base_branch));
        if pr_base == self.base_branch {
            return lines;
        }
        // Once merged, GitHub deletes head branches, so `compare/{trunk}...
        // {branch}` 404s — the merge commit is the ref that still resolves.
        // While the PR is open the branch is the live question and the merge
        // commit is only a speculative test merge.
        let (git_ref, about_the_merge) = match (pr.merged, pr.merge_commit_sha.as_deref()) {
            (true, Some(sha)) => (sha.to_string(), true),
            _ => (pr_base.clone(), false),
        };
        // `trunk...ref`: reachable reads as `identical` or `behind`, and
        // reversing the operands inverts the verdict.
        match github
            .merge_reached_trunk(owner, name, self.base_branch, &git_ref)
            .await
        {
            Ok(reached) => lines.push(match (about_the_merge, reached) {
                (true, true) => format!(
                    "the merge commit {git_ref} IS on {}, so this batch has shipped and \
                     the next poll should retire it",
                    self.base_branch
                ),
                (true, false) => format!(
                    "the merge commit {git_ref} is NOT on {} yet: this PR reached its \
                     base and nothing more, so the batch stays parked until {} lands",
                    self.base_branch, pr_base
                ),
                (false, false) => format!(
                    "its base {} has not reached {} yet, so merging this PR now ships \
                     nothing until {} itself lands",
                    pr_base, self.base_branch, pr_base
                ),
                (false, true) => format!(
                    "its base {} has ALREADY reached {}, so merging this PR now only \
                     adds a commit to a branch nothing will pick up — it wants \
                     retargeting at {} first",
                    pr_base, self.base_branch, self.base_branch
                ),
            }),
            Err(e) => {
                debug!(error = %e, number, "compare unavailable");
                lines.push(format!(
                    "could not check whether {git_ref} has reached {} ({e}) — treat that \
                     as unknown, never as landed",
                    self.base_branch
                ));
            }
        }
        lines
    }

    async fn load_world(&self) -> Result<World, StoreError> {
        let specs = self.store.list_specs().await?;
        let queue = self
            .store
            .list_spec_queue()
            .await?
            .into_iter()
            .map(|item| (item.entry.spec_id.clone(), item.entry))
            .collect();
        let tasks = self
            .store
            .list_tasks()
            .await?
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        let projects = self
            .store
            .list_projects()
            .await?
            .into_iter()
            .map(|project| (project.id.clone(), project))
            .collect();

        // Only builds that are still context: in flight, or finished recently
        // enough to be worth a second look. Which of *those* still claims
        // anything is `is_unresolved`'s question, asked per fact.
        let cutoff = chrono::Utc::now() - chrono::Duration::days(BUILD_RECENCY_DAYS);
        let builds: Vec<Build> = self
            .store
            .list_builds()
            .await?
            .into_iter()
            .filter(|b| match b.status {
                BuildStatus::Queued | BuildStatus::Running => true,
                _ => b.completed_at.is_none_or(|at| at >= cutoff),
            })
            .collect();

        let mut build_specs = HashMap::new();
        for build in &builds {
            let ids = self.store.build_spec_ids(&build.id).await?;
            build_specs.insert(build_key(build).to_string(), ids);
        }
        let superseded = self.store.builds_superseded().await?;

        Ok(World {
            specs,
            queue,
            tasks,
            projects,
            builds,
            build_specs,
            superseded,
        })
    }

    /// Verdicts already rendered on earlier specs for the same task — the
    /// "we have been here before" check. A re-scout arriving with the same
    /// flaw its predecessor was rejected for is the case this catches, and it
    /// is exactly the one a session that lost its context would miss.
    async fn prior_decisions(&self, spec: &Spec, world: &World) -> Result<Vec<String>, StoreError> {
        let siblings: Vec<&Spec> = world
            .specs
            .iter()
            .filter(|s| s.task_id == spec.task_id && s.id != spec.id)
            .collect();
        let mut lines = Vec::new();
        for sibling in siblings {
            let decisions = self
                .store
                .decisions(
                    Some(("spec", sibling.id.as_str())),
                    MAX_PRIOR_DECISIONS as i64,
                )
                .await?;
            for decision in decisions {
                lines.push(format!(
                    "an earlier spec for this task was {} by {} on {}{}",
                    decision.action.as_str(),
                    decision.actor.as_str(),
                    decision.created_at.format("%Y-%m-%d"),
                    match decision.rationale.as_deref() {
                        Some(why) => format!(": {}", first_line(why)),
                        None => String::new(),
                    }
                ));
            }
            if lines.len() >= MAX_PRIOR_DECISIONS {
                break;
            }
        }
        lines.truncate(MAX_PRIOR_DECISIONS);
        Ok(lines)
    }

    /// The half of the brief that has to ask GitHub: whether the PRs this spec
    /// overlaps are still live, and whether it proposes a sequence number the
    /// base branch already uses.
    ///
    /// Best-effort and self-reporting. Every failure path emits a line saying
    /// what could not be checked, so a brief never reads as clean when it is
    /// merely incomplete.
    async fn github_facts(&self, spec: &Spec, world: &World) -> Vec<String> {
        // Nothing to ask about is not the same as not asking: stay silent when
        // this spec opens no PR question and proposes no numbered file, so the
        // "we skipped it" note keeps meaning something when it does appear.
        let overlapping_pr = unresolved_builds(spec, world)
            .iter()
            .any(|(build, _)| build.pr_number.is_some());
        if !overlapping_pr && sequence_candidates(&spec.files_touched).is_empty() {
            return Vec::new();
        }
        let Some(github) = self.github else {
            return vec![
                "GitHub was not consulted (the server has no token): PR state and \
                 base-branch numbering checks were skipped"
                    .into(),
            ];
        };
        let Some(project) = self.project_of(spec, world) else {
            return Vec::new();
        };
        match self
            .within_github_budget(self.live_github_facts(github, project, spec, world))
            .await
        {
            Some(lines) => lines,
            None => {
                warn!(spec = %spec.id, "github facts timed out; briefing without them");
                vec![format!(
                    "GitHub did not answer within the brief's {}s budget: PR state and \
                     base-branch checks were skipped",
                    GITHUB_BUDGET.as_secs()
                )]
            }
        }
    }

    async fn live_github_facts(
        &self,
        github: &GitHubClient,
        project: &Project,
        spec: &Spec,
        world: &World,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let owner = &project.repo_owner;
        let name = &project.repo_name;

        // PR state for every *unresolved* overlapping build that opened one.
        // The filter is what keeps this inside the budget: one request per PR,
        // serially, over every build of the last fortnight was tens of requests
        // and reliably blew [`GITHUB_BUDGET`] — so the brief lost its whole
        // GitHub half exactly when it had the most overlap to explain.
        for (build, _) in unresolved_builds(spec, world) {
            let Some(number) = build.pr_number else {
                continue;
            };
            match github.pull_request_state(owner, name, number).await {
                Ok(state) => lines.push(format!("PR #{number} is {}", state.label())),
                Err(e) => {
                    debug!(error = %e, number, "pr state unavailable");
                    lines.push(format!("PR #{number} state unavailable ({e})"));
                }
            }
        }

        // Sequence-number collisions against the base branch: the migration
        // case. Asked per directory rather than per file, and only for
        // directories where the spec touches a numbered name at all — usually
        // zero requests, occasionally one.
        for (dir, numbered) in sequence_candidates(&spec.files_touched) {
            let existing = match github
                .list_directory(owner, name, &dir, self.base_branch)
                .await
            {
                Ok(entries) => entries,
                Err(e) => {
                    debug!(error = %e, dir, "directory listing unavailable");
                    lines.push(format!(
                        "could not list {dir} on {} to check for numbering clashes ({e})",
                        self.base_branch
                    ));
                    continue;
                }
            };
            let present: HashSet<&str> = existing.iter().map(String::as_str).collect();
            for (basename, prefix) in numbered {
                // Already there under this exact name: the spec is editing an
                // existing file, which is not a clash.
                if present.contains(basename.as_str()) {
                    continue;
                }
                let taken: Vec<&String> = existing
                    .iter()
                    .filter(|e| sequence_prefix(e).as_deref() == Some(prefix.as_str()))
                    .collect();
                if !taken.is_empty() {
                    lines.push(format!(
                        "spec adds {dir}/{basename}, but {} on {} already uses {prefix} ({})",
                        if taken.len() == 1 { "a file" } else { "files" },
                        self.base_branch,
                        taken
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        lines
    }

    /// The repo a spec belongs to, via its task. `None` only if the task or
    /// project row is gone, in which case there is nothing to ask GitHub about.
    fn project_of<'w>(&self, spec: &Spec, world: &'w World) -> Option<&'w Project> {
        let task = world.tasks.get(&spec.task_id)?;
        world.projects.get(&task.project_id)
    }

    /// Human-readable list of the issues a build is carrying.
    fn describe_specs(&self, build_id: &str, world: &World) -> String {
        let Some(ids) = world.build_specs.get(build_id) else {
            return "specs unknown".into();
        };
        let refs: Vec<String> = ids
            .iter()
            .map(|id| match world.specs.iter().find(|s| &s.id == id) {
                Some(spec) => match world.tasks.get(&spec.task_id) {
                    Some(task) => format!("#{}", task.gh_issue_number),
                    None => id.to_string(),
                },
                None => id.to_string(),
            })
            .collect();
        if refs.is_empty() {
            "no specs".into()
        } else {
            refs.join(", ")
        }
    }
}

// --- pure fact functions ---

/// Whether a passing run of the project's own test suite backs this batch.
///
/// It is a **check** and no longer a claim: the Builder *supervisor* runs the
/// suite the project declares at `.tasks/verify`, inside the VM, against the
/// tree the bundle carries — the agent cannot write the answer, and a red suite
/// never became a pull request at all. That is why exactly one of the five
/// states below is green and none of the others is a verdict on the work.
///
/// What a green run does **not** say is the half worth stating on the line: it
/// tested this branch against **its own base**, and the trunk has moved since.
/// Two branches can each be green against their own base and red composed. Only
/// a run of the merged result answers that, and nothing in the pipeline makes
/// one — which is what the landing section sends the reader to do.
///
/// What a base branch means for landing, in the words a reader needs: the
/// stacked case spells out that merging ships nothing, and the unstacked one
/// says merging ships the work — which is what makes the stacked warning worth
/// reading at all.
///
/// One function, because the sentence is reached from two places with different
/// *sources* for `base`: GitHub's live `base_ref` wherever a pull request could
/// be read, and `builds.base_branch` only where it could not. Two hand-written
/// copies of one question is how the poller and the brief came to disagree
/// about the same pull request (#1035).
fn base_verdict(base: &str, trunk: &str) -> String {
    if base == trunk {
        format!("its base IS the trunk ({trunk}), so merging the PR ships the work")
    } else {
        format!(
            "its base is NOT the trunk ({trunk}): merging this PR only moves the work onto \
             {base}, and the tasks stay parked until {base} itself reaches the trunk. Merge \
             the base first, or merge this one and then land {base} — either order works, \
             but neither is done until the commit is on {trunk}"
        )
    }
}

/// [`base_verdict`] for a base nobody could confirm, hedged in front.
///
/// `builds.base_branch` is what the build was **cloned against**, and nothing
/// updates it when a pull request is retargeted — so it is offered here only
/// because GitHub could not be asked, and it is labelled as the build's record
/// rather than as the pull request's base. Reading the column as the live
/// answer is precisely #1035; saying which one this is costs a clause.
fn recorded_base_verdict(base: &str, trunk: &str) -> String {
    format!(
        "as recorded, {} — that is the build's own base, not GitHub's, so a retarget since \
         would not show here",
        base_verdict(base, trunk)
    )
}

/// It says "no automated check" rather than "nothing downstream", and the
/// narrowing is necessary: on a host where the orchestrator has a warm build
/// directory, the *reader* of this brief can go and make a run, even though the
/// *pipeline* still will not make one for it. Leaving a sentence here that says
/// the run will not happen, beside a landing section that says go and make one,
/// is two sources of truth about the same fact.
fn verification_line(verification: Option<&Verification>) -> String {
    let Some(v) = verification else {
        return "no test run is on record at all: this build predates the supervisor's own \
                run, or ran in a Builder image that has not been rebuilt for it — unknown \
                rather than known-skipped, and never a pass"
            .to_string();
    };
    let detail = or_unspecified(&v.detail);
    match v.status {
        VerificationStatus::Passed => format!(
            "the supervisor ran this project's own test suite against the branch and it \
             PASSED ({detail}) — a check rather than the build's claim about itself, and a \
             failing suite would never have opened the pull request. It says nothing about \
             whether the branch still passes composed with a trunk that has moved since its \
             base"
        ),
        VerificationStatus::Undeclared => format!(
            "the project declares no test suite at `.tasks/verify` ({detail}), so nothing ran \
             and no passing run backs this batch — no automated check will make one"
        ),
        VerificationStatus::Unavailable => format!(
            "the suite could not be run ({detail}), so no passing run backs this batch and no \
             automated check will make one"
        ),
        VerificationStatus::TimedOut => format!(
            "the suite did not finish inside its budget and was killed ({detail}), so no \
             passing run backs this batch — the suite never reported on the work either way, \
             which is why the branch shipped rather than failing"
        ),
    }
}

/// Whether a build that was *given* review feedback said anything about it.
///
/// Silent when no spec in the batch carried any — there is nothing to have
/// accounted for, and a standing line saying so is a line that gets skimmed.
/// Blank feedback counts as none, the same rule `BatchItem::new` applies on the
/// way into the prompt: a build cannot account for an empty section it was
/// never shown.
///
/// Unlike [`verification_line`], which is now a check, what this reports is
/// still the build's **own claim** —
/// [`summary_accounts_for_review_feedback`] is a presence check and cannot tell
/// a real accounting from a bare heading. And it is a *fact*, never a veto:
/// `orchestrator::landing_section` names exactly three carve-outs, all about
/// whether a change can be verified, and a brief fact that reads like a fourth
/// would be a second source of truth about who decides.
fn review_feedback_line(build: &Build, world: &World) -> Option<String> {
    let carried = world
        .build_specs
        .get(build_key(build))
        .into_iter()
        .flatten()
        .filter_map(|spec_id| world.queue.get(spec_id))
        .any(|entry| {
            entry
                .feedback
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        });
    if !carried {
        return None;
    }
    Some(
        match summary_accounts_for_review_feedback(build.summary.as_deref()) {
            true => {
                "this batch's specs were approved WITH review feedback, and the build's summary \
             has a `Review feedback` section — that is the build's own claim to have \
             accounted for each item, not a check that it did. Read it against the feedback \
             itself (`GET /spec-queue` for these specs)"
                    .to_string()
            }
            false => {
                "this batch's specs were approved WITH review feedback, and the build's summary \
             has no `Review feedback` section, so whether those items landed is unknown \
             rather than known-skipped. Read the feedback (`GET /spec-queue` for these \
             specs) against the diff and say what you find on the PR — on its own this is \
             not a reason to refuse the merge"
                    .to_string()
            }
        },
    )
}

/// How much of this batch nothing runnable here could have checked.
///
/// A count rather than a verdict. `app-gpui` compiles and unit-tests on a Linux
/// builder (`make app-check`, `make app-test`) — what does not is the
/// rendering, which is why this measures the *surface* and leaves who acts on
/// it to the prompt.
fn verification_surface(files: &[String]) -> String {
    let total = files.len();
    if total == 0 {
        return "no changed files are recorded on this build, so what it touches — and \
                therefore what could have checked it — is unknown"
            .to_string();
    }
    let visual = files
        .iter()
        .filter(|f| f.starts_with(VISUAL_SURFACE))
        .count();
    match visual {
        0 => format!(
            "none of the {total} changed file(s) are under `{VISUAL_SURFACE}`, so all of \
             this is reachable by `make test`"
        ),
        n if n == total => format!(
            "all {total} changed file(s) are under `{VISUAL_SURFACE}`: that compiles and \
             unit-tests here (`make app-check`, `make app-test`), but those tests are \
             pure functions over view state — whether anything actually rendered takes \
             a Mac"
        ),
        n => format!(
            "{n} of {total} changed file(s) are under `{VISUAL_SURFACE}`: the other \
             {} are reachable by `make test`, and the app half compiles and unit-tests \
             here (`make app-check`, `make app-test`) — only whether it rendered takes \
             a Mac",
            total - n
        ),
    }
}

/// Agents leave the detail off sometimes; an empty parenthesis reads as a bug.
fn or_unspecified(detail: &str) -> &str {
    if detail.trim().is_empty() {
        "no command named"
    } else {
        detail
    }
}

/// Other live specs claiming the same files. The duplicate-work check, in the
/// form that catches two scouts sent at overlapping problems.
///
/// Specs for the *same* task are excluded: those are re-scouts, and the prior
/// verdicts on them are reported separately and more usefully.
fn spec_overlap(spec: &Spec, world: &World) -> Vec<String> {
    let mut lines = Vec::new();
    for other in &world.specs {
        if other.id == spec.id || other.task_id == spec.task_id {
            continue;
        }
        let status = match world.queue.get(&other.id) {
            Some(entry) => entry.status,
            None => continue,
        };
        if !matches!(
            status,
            SpecQueueStatus::PendingReview | SpecQueueStatus::Approved
        ) {
            continue;
        }
        let shared = shared_files(&spec.files_touched, &other.files_touched);
        if shared.is_empty() {
            continue;
        }
        let issue = world
            .tasks
            .get(&other.task_id)
            .map(|t| format!("#{}", t.gh_issue_number))
            .unwrap_or_else(|| other.task_id.to_string());
        lines.push(format!(
            "shares {} with spec {} for {issue} ({}): {}",
            plural_files(shared.len()),
            other.id,
            status.as_str(),
            name_some(&shared),
        ));
    }
    lines
}

/// Files this spec claims that a build is *still* claiming. Which PRs are
/// actually open is answered separately, and from GitHub.
///
/// Settled builds are counted, not listed. See [`unresolved_builds`] for why
/// listing them was the bug: a repo that ships thirty PRs a fortnight produced
/// thirty lines of history above the one live conflict, and the count carries
/// the only thing they were jointly worth — that these files move often.
fn build_overlap(spec: &Spec, world: &World) -> Vec<String> {
    let mut lines: Vec<String> = unresolved_builds(spec, world)
        .into_iter()
        .map(|(build, shared)| {
            let where_ = match (build.status, build.pr_number) {
                (BuildStatus::Queued | BuildStatus::Running, _) => {
                    format!("build {} ({})", build.id, build.status.as_str())
                }
                (_, Some(number)) => format!("PR #{number} (build {})", build.id),
                (status, None) => format!("build {} ({})", build.id, status.as_str()),
            };
            format!(
                "shares {} with {where_}: {}",
                plural_files(shared.len()),
                name_some(&shared),
            )
        })
        .collect();

    let settled = settled_overlap_count(spec, world);
    if settled > 0 {
        lines.push(format!(
            "{settled} settled build(s) of the last {BUILD_RECENCY_DAYS} days also touched \
             these files — merged or abandoned, so this is the trunk the spec was written \
             against rather than a competing claim on it, and they are not listed"
        ));
    }
    lines
}

/// Builds whose claim on this spec's files is still live, with the paths they
/// share. One definition, because the overlap lines, the settled count and the
/// PR-state lookups must not disagree about which builds those are.
///
/// A build in flight is obviously live. A finished one is live only while its
/// pull request is unresolved — and *that is a Tasks-owned fact*, so it is read
/// here rather than asked of GitHub: [`Store::finalize_build_succeeded`] parks
/// the batch in `awaiting_merge`, and [`crate::run::watch_merges`] is the only
/// thing that moves it out, to `done` once the commit reaches the trunk or back
/// to `ready_to_build` when the PR closed unmerged. So a task still sitting in
/// `awaiting_merge` is "nobody has resolved this PR yet", with nothing
/// GitHub-owned persisted to say it — **once you have subtracted the builds a
/// later one superseded**. That clause was missing and is what made the
/// sentence quietly wrong: a rebuild parks the same tasks back in
/// `awaiting_merge`, so every earlier build carrying those specs read as
/// unresolved forever. Supersession is Tasks-owned and needs no GitHub read
/// either; see [`Store::builds_superseded`].
///
/// Everything else is history, and history was drowning the signal. The 14-day
/// window was standing in for "its PR is plausibly still open", which is a fine
/// proxy in a repo that ships weekly and a useless one here.
///
/// A failed build never opened a PR and its specs went back to `approved`, so
/// it claims nothing; what actually went wrong is [`Brief::for_blocked_spec`]'s
/// job, and reporting it as overlap would only compete with that.
fn unresolved_builds<'w>(spec: &Spec, world: &'w World) -> Vec<(&'w Build, Vec<String>)> {
    world
        .builds
        .iter()
        .filter(|build| is_unresolved(build, world))
        .filter_map(|build| {
            let shared = shared_files(&spec.files_touched, &build.files_touched);
            (!shared.is_empty()).then_some((build, shared))
        })
        .collect()
}

/// How many builds overlap that [`unresolved_builds`] deliberately dropped.
fn settled_overlap_count(spec: &Spec, world: &World) -> usize {
    world
        .builds
        .iter()
        .filter(|build| !is_unresolved(build, world))
        .filter(|build| !shared_files(&spec.files_touched, &build.files_touched).is_empty())
        .count()
}

/// Whether this build's work is still an open claim — see [`unresolved_builds`].
///
/// Matched exhaustively on purpose: a new [`BuildStatus`] is a new answer to
/// "does this still claim its files", and a wildcard would quietly pick one.
fn is_unresolved(build: &Build, world: &World) -> bool {
    match build.status {
        BuildStatus::Queued | BuildStatus::Running => true,
        // Neither reached a pull request, and both hand the specs back
        // `approved` — a failed build for another attempt, a cancelled one
        // because somebody stopped it. Nothing is holding the files.
        BuildStatus::Failed | BuildStatus::Cancelled => false,
        // A later succeeded build carried every one of its specs, so those
        // files are that build's claim now and this one's PR is nobody's
        // business. Read first, because the test below is `is any task of mine
        // still awaiting_merge` — which names a *parking* and not the build
        // that caused it, and a rebuild re-parks the same tasks. That is the
        // #956/#959 mis-attribution, arriving here through a third reader.
        BuildStatus::Succeeded if world.superseded.contains(build_key(build)) => false,
        BuildStatus::Succeeded => {
            let states: Vec<TaskState> = world
                .build_specs
                .get(build_key(build))
                .into_iter()
                .flatten()
                .filter_map(|spec_id| world.specs.iter().find(|s| &s.id == spec_id))
                .filter_map(|spec| world.tasks.get(&spec.task_id))
                .map(|task| task.state)
                .collect();
            // Nothing left to read a verdict off: a PR nobody can account for
            // is reported, not assumed settled. Dropping it silently is the
            // failure this whole file is written against.
            states.is_empty() || states.contains(&TaskState::AwaitingMerge)
        }
    }
}

/// Two live specs reaching for the same sequence number. The base-branch half
/// of this check needs GitHub; this half does not, and catches the case where
/// both colliding files are still only proposed.
fn sequence_clash_in_flight(spec: &Spec, world: &World) -> Vec<String> {
    let mut mine: HashMap<(String, String), String> = HashMap::new();
    for (dir, numbered) in sequence_candidates(&spec.files_touched) {
        for (basename, prefix) in numbered {
            mine.insert((dir.clone(), prefix), basename);
        }
    }
    if mine.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for other in &world.specs {
        if other.id == spec.id {
            continue;
        }
        let live = matches!(
            world.queue.get(&other.id).map(|entry| entry.status),
            Some(SpecQueueStatus::PendingReview) | Some(SpecQueueStatus::Approved)
        );
        if !live {
            continue;
        }
        for (dir, numbered) in sequence_candidates(&other.files_touched) {
            for (basename, prefix) in numbered {
                if let Some(ours) = mine.get(&(dir.clone(), prefix.clone()))
                    && ours != &basename
                {
                    lines.push(format!(
                        "spec adds {dir}/{ours} while spec {} also adds {dir}/{basename} — \
                         both claim {prefix}",
                        other.id
                    ));
                }
            }
        }
    }
    lines
}

/// Group a file list into `directory -> [(basename, sequence prefix)]`, keeping
/// only names that carry one.
fn sequence_candidates(files: &[String]) -> Vec<(String, Vec<(String, String)>)> {
    let mut by_dir: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for path in files {
        let (dir, basename) = match path.rsplit_once('/') {
            Some((dir, base)) => (dir.to_string(), base.to_string()),
            None => continue, // repo root: no directory to compare within
        };
        if let Some(prefix) = sequence_prefix(&basename) {
            by_dir.entry(dir).or_default().push((basename, prefix));
        }
    }
    let mut out: Vec<(String, Vec<(String, String)>)> = by_dir.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic briefs
    out
}

/// The leading run of digits in a filename, when it is long enough to be a
/// deliberate sequence number and is followed by something else.
fn sequence_prefix(basename: &str) -> Option<String> {
    let digits: String = basename.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() >= MIN_SEQUENCE_DIGITS && digits.len() < basename.len() {
        Some(digits)
    } else {
        None
    }
}

fn shared_files(a: &[String], b: &[String]) -> Vec<String> {
    let set: HashSet<&str> = b.iter().map(String::as_str).collect();
    let mut shared: Vec<String> = a
        .iter()
        .filter(|f| set.contains(f.as_str()))
        .cloned()
        .collect();
    shared.sort();
    shared
}

fn plural_files(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

/// Name the first few paths, then say how many more there are — enough to
/// recognize the overlap without pasting a file list into the window.
fn name_some(files: &[String]) -> String {
    if files.len() <= MAX_NAMED_FILES {
        return files.join(", ");
    }
    format!(
        "{}, +{} more",
        files[..MAX_NAMED_FILES].join(", "),
        files.len() - MAX_NAMED_FILES
    )
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(159).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Builds are keyed by their id's string form in [`World::build_specs`].
fn build_key(build: &Build) -> &str {
    build.id.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_prefixes_are_deliberate_numbers_only() {
        assert_eq!(
            sequence_prefix("0009_watermark.sql").as_deref(),
            Some("0009")
        );
        assert_eq!(sequence_prefix("001-first.md").as_deref(), Some("001"));
        // Too few digits to be a scheme — this is just a name starting with a
        // digit, and treating it as one would fire on unrelated files.
        assert_eq!(sequence_prefix("2fa_login.rs"), None);
        assert_eq!(sequence_prefix("12_thing.rs"), None);
        // All digits and nothing else: no name to collide over.
        assert_eq!(sequence_prefix("0009"), None);
        assert_eq!(sequence_prefix("store.rs"), None);
    }

    #[test]
    fn sequence_candidates_group_by_directory_and_ignore_the_rest() {
        let files = vec![
            "crates/tasks/migrations/0009_thing.sql".to_string(),
            "crates/tasks/migrations/0010_other.sql".to_string(),
            "crates/tasks/src/store.rs".to_string(),
            // Repo root: no directory to compare within, so not a candidate.
            "0009_stray.sql".to_string(),
        ];
        let candidates = sequence_candidates(&files);
        assert_eq!(candidates.len(), 1);
        let (dir, numbered) = &candidates[0];
        assert_eq!(dir, "crates/tasks/migrations");
        assert_eq!(numbered.len(), 2);
        assert!(numbered.contains(&("0009_thing.sql".into(), "0009".into())));
    }

    #[test]
    fn overlap_is_set_intersection_and_names_only_a_few() {
        let a: Vec<String> = ["c.rs", "a.rs", "b.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: Vec<String> = ["b.rs", "a.rs", "z.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(shared_files(&a, &b), vec!["a.rs", "b.rs"]);

        let many: Vec<String> = (0..7).map(|i| format!("f{i}.rs")).collect();
        let named = name_some(&many);
        assert!(named.contains("f0.rs"), "{named}");
        assert!(named.ends_with("+3 more"), "{named}");
        assert!(!named.contains("f6.rs"), "{named}");
    }

    #[test]
    fn a_brief_with_nothing_in_it_renders_nothing() {
        assert!(Brief::render(&[("On spec_1:".into(), vec![])]).is_none());
        let rendered = Brief::render(&[
            (
                "On spec_1:".into(),
                vec!["shares 2 files with PR #4".into()],
            ),
            ("In flight:".into(), vec![]),
        ])
        .expect("a section with lines renders");
        assert!(rendered.starts_with("[brief]"));
        assert!(rendered.contains("shares 2 files with PR #4"));
        // An empty section is omitted rather than rendered as a bare heading.
        assert!(!rendered.contains("In flight:"));
    }

    #[test]
    fn rationale_recall_is_one_line_and_bounded() {
        let long = format!("first line\n{}", "x".repeat(500));
        assert_eq!(first_line(&long), "first line");
        assert!(first_line(&"y".repeat(500)).chars().count() <= 160);
    }

    /// Four cases and never silent — the count is the fact, and the "takes a
    /// Mac" clause belongs only to the part that really does.
    #[test]
    fn the_verification_surface_is_counted_and_never_overstated() {
        let files =
            |paths: &[&str]| -> Vec<String> { paths.iter().map(|p| p.to_string()).collect() };

        let none = verification_surface(&[]);
        assert!(none.contains("no changed files are recorded"), "{none}");

        let server = verification_surface(&files(&["crates/tasks/src/run.rs"]));
        assert!(server.contains("make test"), "{server}");
        assert!(
            !server.contains("Mac"),
            "nothing here needs one, and saying so teaches the rule to be ignored: {server}"
        );

        let app = verification_surface(&files(&[
            "app-gpui/src/sections/queue.rs",
            "app-gpui/src/lib.rs",
        ]));
        assert!(app.contains("Mac"), "{app}");
        // …and it must say what *can* be checked here, or the rule overstates
        // the gap: #877/#893 put the packages in the images on purpose.
        assert!(app.contains("make app-test"), "{app}");
        assert!(app.contains("make app-check"), "{app}");

        let mixed = verification_surface(&files(&[
            "crates/tasks/src/brief.rs",
            "app-gpui/src/sections/queue.rs",
            "crates/tasks/src/run.rs",
        ]));
        assert!(mixed.contains("1 of 3"), "{mixed}");
        assert!(mixed.contains("make test"), "{mixed}");
        assert!(mixed.contains("Mac"), "{mixed}");
    }

    fn verified(status: VerificationStatus) -> Verification {
        Verification {
            status,
            detail: "make test-ci (gate abc1234, same as main)".into(),
        }
    }

    /// Five states, each reading differently — and only one of them green.
    ///
    /// There is deliberately no red state to test: a failing suite fails the
    /// build inside the VM, so a brief can never be handed one.
    #[test]
    fn the_verification_line_reads_differently_in_all_five_states() {
        let passed = verification_line(Some(&verified(VerificationStatus::Passed)));
        assert!(passed.contains("PASSED"), "{passed}");
        assert!(passed.contains("make test-ci"), "{passed}");
        // The two halves the reviewer required: it is a check rather than a
        // claim, AND it is silent about composing with a trunk that moved.
        assert!(passed.contains("a check rather than"), "{passed}");
        assert!(passed.contains("trunk that has moved"), "{passed}");

        let undeclared = verification_line(Some(&verified(VerificationStatus::Undeclared)));
        assert!(
            undeclared.contains("declares no test suite"),
            "{undeclared}"
        );
        assert!(undeclared.contains("no passing run"), "{undeclared}");

        let unavailable = verification_line(Some(&verified(VerificationStatus::Unavailable)));
        assert!(unavailable.contains("could not be run"), "{unavailable}");
        assert!(unavailable.contains("no passing run"), "{unavailable}");

        let timed_out = verification_line(Some(&verified(VerificationStatus::TimedOut)));
        assert!(timed_out.contains("did not finish"), "{timed_out}");
        assert!(timed_out.contains("no passing run"), "{timed_out}");

        let absent = verification_line(None);
        assert!(absent.contains("no test run is on record"), "{absent}");
        assert!(
            absent.contains("unknown rather than known-skipped"),
            "{absent}"
        );
        assert!(absent.contains("never a pass"), "{absent}");

        // No two of them read alike — a reader must be able to tell "declares
        // nothing" from "was killed" from "no image support" at a glance.
        let lines = [passed, undeclared, unavailable, timed_out, absent];
        for (i, a) in lines.iter().enumerate() {
            for b in lines.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Exactly one state may describe the batch as backed by a passing run.
    /// Every other line has to say, in words, that none backs it.
    #[test]
    fn only_the_passing_state_claims_a_run_backs_the_batch() {
        for status in [
            VerificationStatus::Undeclared,
            VerificationStatus::Unavailable,
            VerificationStatus::TimedOut,
        ] {
            let line = verification_line(Some(&verified(status)));
            assert!(
                line.contains("no passing run backs this batch"),
                "{status}: {line}"
            );
        }
        assert!(!verification_line(None).contains("PASSED"));
    }

    /// #935's other half: the prompt now carries the feedback, and this is the
    /// only thing downstream that says whether it was answered. Silent when the
    /// batch was approved with nothing, because a standing line saying "no
    /// feedback to account for" is a line that gets skimmed.
    #[test]
    fn the_review_feedback_line_reports_the_claim_and_is_silent_without_feedback() {
        let build = a_build();
        let spec_id = SpecId::new();

        let world = |feedback: Option<&str>| a_world(&build, &spec_id, feedback);

        // Nothing was required of this batch, so there is nothing to report —
        // and blank text in the column reads the same, since the prompt does
        // not render an empty section for it either.
        assert_eq!(review_feedback_line(&build, &world(None)), None);
        assert_eq!(review_feedback_line(&build, &world(Some("  \n "))), None);

        let carried = world(Some("name the constant"));

        let mut answered = build.clone();
        answered.summary = Some("Did it.\n\n## Review feedback\n\n- Renamed it.".into());
        let line = review_feedback_line(&answered, &carried).expect("a line");
        assert!(line.contains("has a `Review feedback` section"), "{line}");
        assert!(
            line.contains("the build's own claim"),
            "a presence check is never described as a check: {line}"
        );
        assert!(line.contains("GET /spec-queue"), "{line}");

        let mut silent = build.clone();
        silent.summary = Some("Did it.".into());
        let line = review_feedback_line(&silent, &carried).expect("a line");
        assert!(line.contains("no `Review feedback` section"), "{line}");
        assert!(line.contains("unknown rather than known-skipped"), "{line}");
        assert!(
            line.ends_with("on its own this is not a reason to refuse the merge"),
            "a fact must not read as a fourth landing carve-out: {line}"
        );
    }

    fn a_build() -> Build {
        Build {
            id: crate::models::BuildId::new(),
            project_id: ProjectId::new(),
            vm_id: None,
            branch: "build/x".into(),
            base_branch: "main".into(),
            base_sha: None,
            head_sha: None,
            pr_number: Some(4),
            status: BuildStatus::Succeeded,
            summary: None,
            files_touched: vec![],
            exit_reason: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            agent_finished_at: None,
            completed_at: Some(chrono::Utc::now()),
            directions: None,
            verification: None,
        }
    }

    /// The smallest world [`review_feedback_line`] reads: one build carrying
    /// one spec, whose queue entry holds `feedback`.
    fn a_world(build: &Build, spec_id: &SpecId, feedback: Option<&str>) -> World {
        World {
            specs: Vec::new(),
            queue: HashMap::from([(
                spec_id.clone(),
                SpecQueueEntry {
                    spec_id: spec_id.clone(),
                    status: SpecQueueStatus::Approved,
                    rank: None,
                    approved_at: None,
                    feedback: feedback.map(str::to_string),
                    blocking_dependencies: vec![],
                },
            )]),
            tasks: HashMap::new(),
            projects: HashMap::new(),
            builds: Vec::new(),
            build_specs: HashMap::from([(build_key(build).to_string(), vec![spec_id.clone()])]),
            superseded: HashSet::new(),
        }
    }

    /// A stale image is a **fact**, not an obligation — the orchestrator holds
    /// a curl-only token in a VM-less workdir and can never discharge one — so
    /// it rides the pipeline brief, worded around the judgment it *can* make:
    /// a run that failed inside a stale image has not told you anything about
    /// its task. That is the reading #884 got wrong.
    #[tokio::test]
    async fn a_stale_image_is_a_pipeline_fact_and_a_current_one_is_silent() {
        use crate::protocol::SupervisorBuild;
        use tasks_api::version::ImageRole;

        let store = Store::open_in_memory().await.unwrap();
        let brief = Brief::new(&store, None, "main");

        // Nothing observed: silent here. "None observed yet" belongs on the
        // standing /status line, not in a brief that claims to report facts.
        assert!(brief.stale_image_facts().await.unwrap().is_empty());

        // An image at this very build is current, and also silent.
        store
            .record_image_build(
                "agent:v1",
                ImageRole::Scout,
                Some(&SupervisorBuild {
                    version: crate::version::VERSION.into(),
                    commit: "abc1234".into(),
                }),
                "sess_1",
            )
            .await
            .unwrap();
        assert!(brief.stale_image_facts().await.unwrap().is_empty());

        // One that predates stamping is not.
        store
            .record_image_build("builder:v1", ImageRole::Builder, None, "build_1")
            .await
            .unwrap();
        let facts = brief.stale_image_facts().await.unwrap();
        assert_eq!(facts.len(), 1, "{facts:?}");
        assert!(facts[0].contains("builder:v1"), "{}", facts[0]);
        assert!(
            facts[0].contains("predates supervisor stamping"),
            "{}",
            facts[0]
        );
        assert!(
            facts[0].contains("not a verdict on its task"),
            "the judgment the orchestrator actually makes: {}",
            facts[0]
        );
        assert!(
            facts[0].contains("Only a human at the host can rebuild it"),
            "and it must not read as something the orchestrator could do: {}",
            facts[0]
        );
    }

    /// [`GITHUB_BUDGET`] is documented as the whole brief's, and a per-call
    /// timeout of the same size would let a turn briefing four subjects spend
    /// forty seconds. Asserted on the clock, because a per-call version also
    /// answers `None` — just ten seconds later.
    #[tokio::test]
    async fn the_github_budget_is_spent_once_for_the_whole_brief() {
        // Opened before the clock is paused, deliberately: sqlx's pool acquire
        // is itself on a timer, and under a paused clock it times out
        // instantly. `start_paused` would do it the wrong way round.
        let store = Store::open_in_memory().await.unwrap();
        tokio::time::pause();
        let brief = Brief::new(&store, None, "main");
        let start = tokio::time::Instant::now();

        // Two reads that never answer: the first spends the budget, and the
        // second must cost nothing at all rather than another full window.
        for _ in 0..2 {
            let answered: Option<()> = brief
                .within_github_budget(std::future::pending::<()>())
                .await;
            assert!(answered.is_none());
        }

        // One budget, not two. The margin is tokio's timer granularity, which
        // rounds a deadline up by a millisecond; two budgets would be 20s.
        let spent = start.elapsed();
        assert!(
            spent >= GITHUB_BUDGET && spent < GITHUB_BUDGET + Duration::from_secs(1),
            "the budget bounds the brief, not each call in it: spent {spent:?}"
        );
    }
}
