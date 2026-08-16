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

use tracing::{debug, warn};

use crate::builder::{VerificationReport, verification_report};
use crate::github::GitHubClient;
use crate::models::{
    Build, BuildId, BuildStatus, Project, ProjectId, Spec, SpecId, SpecQueueStatus, Task, TaskId,
    TaskState,
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
    queue: HashMap<SpecId, SpecQueueStatus>,
    tasks: HashMap<TaskId, Task>,
    projects: HashMap<ProjectId, Project>,
    builds: Vec<Build>,
    /// Spec ids per build, for naming what a build is carrying.
    build_specs: HashMap<String, Vec<SpecId>>,
}

impl<'a> Brief<'a> {
    pub fn new(store: &'a Store, github: Option<&'a GitHubClient>, base_branch: &'a str) -> Self {
        Self {
            store,
            github,
            base_branch,
            world: tokio::sync::OnceCell::new(),
            github_deadline: tokio::sync::OnceCell::new(),
        }
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
    pub async fn for_stranded_build(&self, build_id: &BuildId) -> Result<Vec<String>, StoreError> {
        let Some(build) = self.store.get_build(build_id).await? else {
            return Ok(vec![format!(
                "build {build_id} is not in the store — the obligation named a row that is gone"
            )]);
        };
        let world = self.world().await?;
        let mut lines = vec![format!(
            "branch {} merges into {}, and PR #{} carries it",
            build.branch,
            build.base_branch,
            build
                .pr_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(none)".into()),
        )];
        if build.base_branch == self.base_branch {
            lines.push(format!(
                "its base IS the trunk ({}), so merging the PR ships the work",
                self.base_branch
            ));
        } else {
            lines.push(format!(
                "its base is NOT the trunk ({}): merging this PR only moves the work onto {}, \
                 and the tasks stay parked until {} itself reaches the trunk. Merge the base \
                 first, or merge this one and then land {} — either order works, but neither \
                 is done until the commit is on {}",
                self.base_branch,
                build.base_branch,
                build.base_branch,
                build.base_branch,
                self.base_branch,
            ));
        }
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
        lines.push(verification_line(build.summary.as_deref()));
        lines.push(verification_surface(&build.files_touched));
        lines.extend(self.landing_facts(&build, world).await);
        Ok(lines)
    }

    /// What the pipeline is doing right now, in the two or three lines that
    /// change a judgment: what a Builder is holding, and what is already
    /// approved and waiting behind it.
    pub async fn pipeline(&self) -> Result<Vec<String>, StoreError> {
        let world = self.world().await?;
        let mut lines = Vec::new();

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
            .filter(|(spec_id, status)| {
                **status == SpecQueueStatus::Approved && !carried.contains(spec_id)
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
            return vec![format!(
                "GitHub was not consulted (the server has no token): whether PR #{number} \
                 can be merged is unchecked rather than fine"
            )];
        };
        let Some(project) = world.projects.get(&build.project_id) else {
            return vec![format!(
                "the project row for this build is gone, so PR #{number} could not be \
                 looked up"
            )];
        };
        match self
            .within_github_budget(self.live_landing_facts(github, project, build, number))
            .await
        {
            Some(lines) => lines,
            None => {
                warn!(build = %build.id, number, "landing facts timed out; briefing without them");
                vec![format!(
                    "GitHub did not answer within the brief's {}s budget: whether PR \
                     #{number} can be merged is unchecked rather than fine",
                    GITHUB_BUDGET.as_secs()
                )]
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
                return vec![format!(
                    "PR #{number} could not be read ({e}): its mergeability is unknown \
                     rather than fine"
                )];
            }
        };
        let mut lines = vec![format!(
            "PR #{number} is {}: {}",
            pr.label(),
            pr.landing().describe()
        )];

        // The stacked question, and only for a stacked build: an unstacked one
        // has its answer in `base_ref` already and must cost no extra call.
        if build.base_branch == self.base_branch {
            return lines;
        }
        // Once merged, GitHub deletes head branches, so `compare/{trunk}...
        // {branch}` 404s — the merge commit is the ref that still resolves.
        // While the PR is open the branch is the live question and the merge
        // commit is only a speculative test merge.
        let (git_ref, about_the_merge) = match (pr.merged, pr.merge_commit_sha.as_deref()) {
            (true, Some(sha)) => (sha.to_string(), true),
            _ => (build.base_branch.clone(), false),
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
                    self.base_branch, build.base_branch
                ),
                (false, false) => format!(
                    "its base {} has not reached {} yet, so merging this PR now ships \
                     nothing until {} itself lands",
                    build.base_branch, self.base_branch, build.base_branch
                ),
                (false, true) => format!(
                    "its base {} has ALREADY reached {}, so merging this PR now only \
                     adds a commit to a branch nothing will pick up — it wants \
                     retargeting at {} first",
                    build.base_branch, self.base_branch, self.base_branch
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
            .map(|item| (item.entry.spec_id, item.entry.status))
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

        Ok(World {
            specs,
            queue,
            tasks,
            projects,
            builds,
            build_specs,
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

/// What the build said about its own test run, attributed as a claim.
///
/// It *is* a claim — nothing here re-ran anything, and this repository has no
/// workflows and no required checks, so there is no second opinion anywhere in
/// the loop. Saying so on the line is the difference between evidence and the
/// appearance of evidence.
fn verification_line(summary: Option<&str>) -> String {
    match verification_report(summary) {
        VerificationReport::Passed(detail) => format!(
            "the build reported its own tests PASSED ({}) — its claim, not an \
             independent run, and the only evidence here that the change works",
            or_unspecified(&detail),
        ),
        VerificationReport::Failed(detail) => format!(
            "the build reported its own tests FAILED ({}) — its claim, not an \
             independent run, and nothing downstream re-runs them, so no passing \
             run backs this batch",
            or_unspecified(&detail),
        ),
        VerificationReport::NotRun(detail) => format!(
            "the build reported that it did NOT RUN the tests ({}) — so no run at \
             all backs this batch, and nothing downstream will make one",
            or_unspecified(&detail),
        ),
        VerificationReport::Unreported => {
            "the build recorded no verification line, so there is no test run at all on \
             record: whether anything ran is unknown rather than known-skipped \
             (builds from before the line was asked for read the same way)"
                .to_string()
        }
    }
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
            Some(status) => *status,
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
/// `awaiting_merge` *is* "nobody has resolved this PR yet", with nothing
/// GitHub-owned persisted to say it.
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
            world.queue.get(&other.id),
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

    /// The line is a *claim*, and each of the four states has to read
    /// differently — "no line" and "did not run" especially, since one is
    /// compatible with a passing run nobody wrote down and the other is not.
    #[test]
    fn the_verification_line_attributes_the_claim_in_all_four_states() {
        let passed = verification_line(Some("Verification: PASSED — make test"));
        assert!(passed.contains("PASSED"), "{passed}");
        assert!(passed.contains("make test"), "{passed}");
        assert!(
            passed.contains("its claim, not an independent run"),
            "{passed}"
        );

        let failed = verification_line(Some("Verification: FAILED — make test, 2 red"));
        assert!(failed.contains("FAILED"), "{failed}");
        assert!(failed.contains("no passing run"), "{failed}");

        let not_run = verification_line(Some("Verification: NOT RUN — no runner"));
        assert!(not_run.contains("NOT RUN"), "{not_run}");
        assert!(not_run.contains("no run at all"), "{not_run}");

        let silent = verification_line(Some("Just prose."));
        assert!(silent.contains("no test run at all"), "{silent}");
        assert!(
            silent.contains("unknown rather than known-skipped"),
            "{silent}"
        );
        assert_eq!(
            silent,
            verification_line(None),
            "a build from before the line reads the same"
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
